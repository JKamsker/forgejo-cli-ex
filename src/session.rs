use std::{path::Path, sync::Arc, time::Duration};

use cookie_store::CookieStore;
use eyre::{eyre, Context};
use reqwest::redirect::Policy;
use reqwest_cookie_store::CookieStoreMutex;
use url::Url;

use crate::{html, session_cookies, store};

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct UiSession {
    base_url: String,
    cookie_store: Arc<CookieStoreMutex>,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginResult {
    LoggedIn,
    TwoFactorRequired,
}

impl UiSession {
    pub fn base_url(&self) -> &str {
        self.request_base_url()
    }

    // Internal helper: get the base URL for HTTP requests
    // (converts http+unix:// to http://localhost)
    fn request_base_url(&self) -> &str {
        if self.base_url.starts_with("http+unix://") {
            "http://localhost"
        } else {
            &self.base_url
        }
    }

    // Internal helper: get the storage URL (preserves http+unix://)
    fn storage_base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn from_store(base_url: &str, force_relogin: bool) -> eyre::Result<Self> {
        Self::from_store_with_socket(base_url, force_relogin, None).await
    }

    pub async fn from_store_with_socket(
        base_url: &str,
        force_relogin: bool,
        unix_socket: Option<&Path>,
    ) -> eyre::Result<Self> {
        let normalized = crate::target::normalize_base_url(base_url)?;

        // Derive socket from base_url if it's http+unix://
        // This ensures base_url and unix_socket stay in sync
        let derived_socket: Option<std::path::PathBuf>;
        let effective_socket =
            if let Some((socket, _)) = crate::target::parse_unix_socket_url(&normalized) {
                derived_socket = Some(socket);
                derived_socket.as_deref()
            } else {
                unix_socket
            };

        let info = store::get_store_entry(&normalized).await?;
        let entry = info.entry;
        let ui_login = entry.as_ref().and_then(store::StoreEntry::ui_login_kind);

        match ui_login {
            Some(store::CredentialKind::Session) => {
                if force_relogin {
                    return Err(session_expired_error(&normalized));
                }
                let jar = entry.and_then(|e| e.cookie_jar);
                let session = Self::new_with_socket(&normalized, jar.as_ref(), effective_socket)?;
                if session.test_session().await? {
                    let _ = session.persist_cookie_jar().await;
                    return Ok(session);
                }
                return Err(session_expired_error(&normalized));
            }
            Some(store::CredentialKind::Password) => {}
            _ => return Err(no_ui_login_error(&normalized, entry.as_ref())),
        }

        let cookie_jar = if force_relogin {
            None
        } else {
            entry.and_then(|e| e.cookie_jar)
        };

        let session = Self::new_with_socket(&normalized, cookie_jar.as_ref(), effective_socket)?;
        if !force_relogin && session.test_session().await.unwrap_or(false) {
            let _ = session.persist_cookie_jar().await;
            return Ok(session);
        }

        let creds = store::get_ui_creds(&normalized)
            .await?
            .ok_or_else(|| no_ui_login_error(&normalized, None))?;

        session
            .login_with_creds(&creds.username, &creds.password)
            .await?;
        session.persist_cookie_jar_required().await?;
        Ok(session)
    }

    pub fn new(base_url: &str, cookie_jar: Option<&store::CookieJar>) -> eyre::Result<Self> {
        Self::new_with_socket(base_url, cookie_jar, None)
    }

    pub fn new_with_socket(
        base_url: &str,
        cookie_jar: Option<&store::CookieJar>,
        unix_socket: Option<&Path>,
    ) -> eyre::Result<Self> {
        let base_url = crate::target::normalize_base_url(base_url)?;

        // Derive socket from base_url if it's http+unix://
        // This ensures base_url and unix_socket stay in sync
        let derived_socket: Option<std::path::PathBuf>;
        let effective_socket =
            if let Some((socket, _)) = crate::target::parse_unix_socket_url(&base_url) {
                derived_socket = Some(socket);
                derived_socket.as_deref()
            } else {
                unix_socket
            };

        let cookie_store = CookieStoreMutex::new(CookieStore::default());

        if let Some(jar) = cookie_jar {
            session_cookies::load_cookie_jar_into_store(&cookie_store, jar)?;
        }

        let cookie_store = Arc::new(cookie_store);
        let builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::limited(10))
            .timeout(Duration::from_secs(60))
            .cookie_provider(Arc::clone(&cookie_store));

        #[cfg(unix)]
        let builder = if let Some(socket_path) = effective_socket {
            builder.unix_socket(socket_path)
        } else {
            builder
        };

        #[cfg(not(unix))]
        let _ = effective_socket;

        let client = builder.build().wrap_err("failed to build http client")?;

        Ok(Self {
            base_url,
            cookie_store,
            client,
        })
    }

    pub async fn test_session(&self) -> eyre::Result<bool> {
        let settings_url = format!("{}/user/settings", self.request_base_url());
        let resp = self
            .client
            .get(&settings_url)
            .send()
            .await
            .wrap_err("failed to probe session")?;

        if let Some(target) = canonical_host_redirect(self.request_base_url(), resp.url()) {
            // The destination is not verified: it looks like this instance's
            // own settings/login page on another host (a ROOT_URL redirect),
            // but an SSO proxy could use the same path.
            return Err(eyre!(
                "'{}' redirected the session check to another host, '{target}' (unverified). \
                 If that is this instance's configured ROOT_URL, cookies only work there: \
                 rerun with `--host {target}`. Otherwise the instance sends logins off-site \
                 (e.g. SSO); import a browser session or use an API token instead.",
                self.request_base_url()
            ));
        }
        if is_auth_challenge_url(self.request_base_url(), resp.url())
            || is_off_instance_url(self.request_base_url(), resp.url())
        {
            return Ok(false);
        }
        if resp.status().as_u16() >= 400 {
            return Ok(false);
        }
        Ok(true)
    }

    pub async fn relogin_from_store(&self) -> eyre::Result<()> {
        let info = store::get_store_entry(self.storage_base_url()).await?;
        if info
            .entry
            .as_ref()
            .and_then(store::StoreEntry::ui_login_kind)
            == Some(store::CredentialKind::Session)
        {
            // An imported browser session cannot be renewed without a password.
            return Err(session_expired_error(self.storage_base_url()));
        }

        let creds = store::get_ui_creds(self.storage_base_url())
            .await?
            .ok_or_else(|| no_ui_login_error(self.storage_base_url(), info.entry.as_ref()))?;

        store::clear_cookie_jar(self.storage_base_url()).await?;
        {
            let mut guard = self.cookie_store.lock().unwrap();
            guard.clear();
        }

        self.login_with_creds(&creds.username, &creds.password)
            .await?;
        self.persist_cookie_jar_required().await?;
        Ok(())
    }

    /// Snapshot of the session's current cookies.
    pub fn cookie_jar(&self) -> eyre::Result<store::CookieJar> {
        session_cookies::cookie_jar_from_store(&self.cookie_store)
    }

    /// Cookie domain used for requests to this instance (`localhost` for Unix sockets).
    pub fn cookie_domain(&self) -> eyre::Result<String> {
        Url::parse(self.request_base_url())
            .ok()
            .and_then(|u| u.host_str().map(ToOwned::to_owned))
            .ok_or_else(|| eyre!("unable to determine cookie domain for '{}'", self.base_url))
    }

    /// Cookie path the instance uses for its own cookies: its base path
    /// (`/gitea` for `https://host/gitea`), or `/` for a root instance.
    pub fn cookie_path(&self) -> String {
        Url::parse(self.request_base_url())
            .ok()
            .map(|u| u.path().trim_end_matches('/').to_string())
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| "/".to_string())
    }

    /// Whether requests to this instance go over HTTPS.
    pub fn is_https(&self) -> bool {
        self.request_base_url().starts_with("https://")
    }

    /// Login name of the session's user, read from the profile settings form
    /// (the REST API does not accept session cookies).
    pub async fn fetch_login(&self) -> Option<String> {
        let url = format!("{}/user/settings", self.request_base_url());
        let resp = self.client.get(&url).send().await.ok()?;
        if !resp.status().is_success()
            || is_auth_challenge_url(self.request_base_url(), resp.url())
            || is_off_instance_url(self.request_base_url(), resp.url())
        {
            return None;
        }
        let page = resp.text().await.ok()?;
        html::get_settings_username(&page)
    }

    pub async fn login_with_creds(&self, username: &str, password: &str) -> eyre::Result<()> {
        match self
            .login_with_creds_and_otp(username, password, None)
            .await?
        {
            LoginResult::LoggedIn => Ok(()),
            LoginResult::TwoFactorRequired => Err(eyre!(
                "Two-factor authentication is required for '{}' on '{}'. Run `fj-ex auth login --host {}` to refresh the stored UI cookies.",
                username,
                self.storage_base_url(),
                self.storage_base_url()
            )),
        }
    }

    pub async fn login_with_creds_and_otp(
        &self,
        username: &str,
        password: &str,
        otp: Option<&str>,
    ) -> eyre::Result<LoginResult> {
        {
            let mut guard = self.cookie_store.lock().unwrap();
            guard.clear();
        }

        let login_url = format!("{}/user/login", self.request_base_url());
        let login_page = self
            .client
            .get(&login_url)
            .send()
            .await
            .wrap_err("failed to load login page")?;
        if login_page.status() != reqwest::StatusCode::OK {
            return Err(eyre!(
                "Failed to load login page ({}). HTTP {}.",
                login_url,
                login_page.status()
            ));
        }

        let login_html = login_page
            .text()
            .await
            .wrap_err("failed to read login html")?;
        let csrf = html::get_csrf_from_login_html(&login_html);

        let mut form = vec![
            ("user_name", username.to_string()),
            ("password", password.to_string()),
            ("remember", "on".to_string()),
        ];
        if let Some(csrf) = csrf {
            form.push(("_csrf", csrf));
        }

        let login_resp = self
            .client
            .post(&login_url)
            .form(&form)
            .send()
            .await
            .wrap_err("failed to post login form")?;

        if is_two_factor_url(self.request_base_url(), login_resp.url()) {
            let Some(passcode) = otp else {
                return Ok(LoginResult::TwoFactorRequired);
            };

            self.submit_two_factor_code(passcode).await?;
            return Ok(LoginResult::LoggedIn);
        }

        let ok = self.test_session().await?;
        if !ok {
            return Err(eyre!(
                "Login failed for '{}' on '{}' (session validation returned to the login flow).",
                username,
                self.base_url
            ));
        }

        Ok(LoginResult::LoggedIn)
    }

    pub async fn submit_two_factor_code(&self, passcode: &str) -> eyre::Result<()> {
        let passcode = passcode.trim();
        if passcode.is_empty() {
            return Err(eyre!("Two-factor passcode must not be empty."));
        }

        let two_factor_url = format!("{}/user/two_factor", self.request_base_url());
        let csrf = self.load_two_factor_csrf(&two_factor_url).await?;

        let mut form = vec![("passcode", passcode.to_string())];
        if let Some(csrf) = csrf {
            form.push(("_csrf", csrf));
        }

        let resp = self
            .client
            .post(&two_factor_url)
            .form(&form)
            .send()
            .await
            .wrap_err("failed to post two-factor form")?;

        if is_two_factor_url(self.request_base_url(), resp.url()) {
            return Err(eyre!(
                "Two-factor passcode was rejected for '{}'.",
                self.base_url
            ));
        }
        if is_login_url(self.request_base_url(), resp.url()) {
            return Err(eyre!(
                "Login failed for '{}' after two-factor validation.",
                self.base_url
            ));
        }

        let ok = self.test_session().await?;
        if !ok {
            return Err(eyre!(
                "Login failed for '{}' after two-factor validation.",
                self.base_url
            ));
        }

        Ok(())
    }

    async fn load_two_factor_csrf(&self, two_factor_url: &str) -> eyre::Result<Option<String>> {
        let resp = self
            .client
            .get(two_factor_url)
            .send()
            .await
            .wrap_err("failed to load two-factor page")?;

        if resp.status() != reqwest::StatusCode::OK {
            return Ok(None);
        }

        let html = resp
            .text()
            .await
            .wrap_err("failed to read two-factor html")?;

        Ok(html::get_csrf_from_login_html(&html))
    }

    pub async fn get_text(&self, url: &str, retry_on_logout: bool) -> eyre::Result<String> {
        self.get_response(url, retry_on_logout)
            .await?
            .text()
            .await
            .wrap_err("failed to read response text")
    }

    pub async fn get_bytes(&self, url: &str, retry_on_logout: bool) -> eyre::Result<Vec<u8>> {
        self.get_response(url, retry_on_logout)
            .await?
            .bytes()
            .await
            .wrap_err("failed to read response bytes")
            .map(|b| b.to_vec())
    }

    pub async fn post_json_text(
        &self,
        url: &str,
        body: &serde_json::Value,
        retry_on_logout: bool,
    ) -> eyre::Result<String> {
        self.post_json_response(url, body, retry_on_logout)
            .await?
            .text()
            .await
            .wrap_err("failed to read response text")
    }

    pub async fn get_response(
        &self,
        url: &str,
        retry_on_logout: bool,
    ) -> eyre::Result<reqwest::Response> {
        self.send_with_retry(retry_on_logout, || self.client.get(url))
            .await
    }

    pub async fn post_json_response(
        &self,
        url: &str,
        body: &serde_json::Value,
        retry_on_logout: bool,
    ) -> eyre::Result<reqwest::Response> {
        self.send_with_retry(retry_on_logout, || {
            let request = self.client.post(url).json(body);
            // Servers that still use token-based CSRF protection (e.g. Gitea
            // <= 1.25) mirror the token in the `_csrf` cookie and reject UI
            // POSTs without the matching header. Newer servers ignore it.
            match self.csrf_cookie(url) {
                Some(token) => request.header("X-Csrf-Token", token),
                None => request,
            }
        })
        .await
    }

    /// Value of the `_csrf` cookie that would be sent to `url`, if any.
    fn csrf_cookie(&self, url: &str) -> Option<String> {
        let url = Url::parse(url).ok()?;
        let store = self.cookie_store.lock().ok()?;
        csrf_cookie_value(&store, &url)
    }

    pub async fn post_json_response_with_csrf(
        &self,
        url: &str,
        body: &serde_json::Value,
        csrf_token: &str,
        retry_on_logout: bool,
    ) -> eyre::Result<reqwest::Response> {
        self.send_with_retry(retry_on_logout, || {
            self.client
                .post(url)
                .header("X-Csrf-Token", csrf_token)
                .json(body)
        })
        .await
    }

    pub async fn persist_cookie_jar(&self) -> eyre::Result<()> {
        let jar = session_cookies::cookie_jar_from_store(&self.cookie_store)?;
        store::save_cookie_jar(self.storage_base_url(), jar).await?;
        Ok(())
    }

    pub async fn persist_cookie_jar_required(&self) -> eyre::Result<()> {
        let jar = session_cookies::cookie_jar_from_store(&self.cookie_store)?;
        store::save_cookie_jar_required(self.storage_base_url(), jar).await?;
        Ok(())
    }

    async fn send_with_retry(
        &self,
        retry_on_logout: bool,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> eyre::Result<reqwest::Response> {
        let resp = build().send().await.wrap_err("request failed")?;
        if retry_on_logout && is_auth_challenge_url(self.request_base_url(), resp.url()) {
            self.relogin_from_store().await?;
            let resp = build().send().await.wrap_err("request failed")?;
            self.persist_cookie_jar_required().await?;
            return Ok(resp);
        }

        self.persist_cookie_jar().await?;
        Ok(resp)
    }
}

/// The `_csrf` cookie for `url`. When several match (e.g. an imported `Path=/`
/// cookie next to a server-set one at the instance sub-path), the one with the
/// longest path wins, as in RFC 6265 cookie ordering; `matches` itself returns
/// cookies in no particular order.
fn csrf_cookie_value(store: &CookieStore, url: &Url) -> Option<String> {
    store
        .matches(url)
        .into_iter()
        .filter(|cookie| cookie.name() == "_csrf" && !cookie.value().is_empty())
        .max_by_key(|cookie| AsRef::<str>::as_ref(&cookie.path).len())
        .map(|cookie| cookie.value().to_string())
}

/// Error shown when an imported browser session no longer authenticates.
pub fn session_expired_error(base_url: &str) -> eyre::Report {
    eyre!(
        "The imported browser session for '{base_url}' has expired (the server redirected to the login page).\n\
         Imported sessions cannot be renewed automatically. Either re-import a fresh session cookie:\n  \
         fj-ex auth login --host {base_url} --session-cookie-stdin\n\
         or use an API token instead:\n  \
         fj-ex auth login --host {base_url} --token-stdin   # or export FJ_EX_TOKEN / FORGEJO_TOKEN / GITEA_TOKEN"
    )
}

/// Error shown when a command needs a UI session but none is stored.
pub fn no_ui_login_error(base_url: &str, entry: Option<&store::StoreEntry>) -> eyre::Report {
    let token_only = entry.is_some_and(store::StoreEntry::has_api_token);
    if token_only {
        return eyre!(
            "No UI session for '{base_url}': only an API token is stored, and this command needs the web UI.\n\
             Log in with `fj-ex auth login --host {base_url}` (password) or import a browser session with \
             `fj-ex auth login --host {base_url} --session-cookie-stdin`."
        );
    }
    eyre!(
        "No stored UI creds for '{base_url}'. Run `fj-ex auth login --host {base_url}` (password), \
         `fj-ex auth login --host {base_url} --session-cookie-stdin` (SSO browser session), \
         or provide an API token (`fj-ex auth login --host {base_url} --token-stdin`, or FJ_EX_TOKEN/FORGEJO_TOKEN/GITEA_TOKEN)."
    )
}

fn is_auth_challenge_url(base_url: &str, url: &Url) -> bool {
    is_login_url(base_url, url) || is_two_factor_url(base_url, url)
}

/// Whether a (redirected) response URL left the instance: another host, a
/// different scheme or port (except an `http` -> `https` upgrade on default
/// ports), or a path outside the instance base path. SSO deployments redirect
/// unauthenticated requests to an identity provider or forward-auth login
/// page, which answers 200 but is not a logged-in instance page.
fn is_off_instance_url(base_url: &str, url: &Url) -> bool {
    let Ok(base) = Url::parse(base_url) else {
        return false;
    };
    if base.host_str() != url.host_str() {
        return true;
    }
    let same_origin = base.scheme() == url.scheme() && base.port() == url.port();
    let https_upgrade = base.scheme() == "http"
        && url.scheme() == "https"
        && base.port().is_none()
        && url.port().is_none();
    if !same_origin && !https_upgrade {
        return true;
    }
    let base_path = base.path().trim_end_matches('/');
    !base_path.is_empty() && !path_is_or_under(url.path(), base_path)
}

/// The origin (plus base path) a probe of `/user/settings` was redirected to
/// when the final page looks like the instance's own settings or login page
/// on another origin, as after a redirect to the canonical ROOT_URL host.
/// This is judged by path only, so callers must present it as unverified.
fn canonical_host_redirect(base_url: &str, url: &Url) -> Option<String> {
    if !is_off_instance_url(base_url, url) {
        return None;
    }
    let relative = instance_relative_path(base_url, url);
    if !path_is_or_under(relative, "/user/settings") && !path_is_or_under(relative, "/user/login") {
        return None;
    }
    let path = url.path();
    let base_path = path.strip_suffix(relative).unwrap_or_default();
    let mut canonical = url.clone();
    canonical.set_path(base_path);
    canonical.set_query(None);
    canonical.set_fragment(None);
    Some(canonical.as_str().trim_end_matches('/').to_string())
}

/// Matches `/user/login` relative to the instance base path, so both
/// `https://host/user/login` and `https://host/gitea/user/login` (for a
/// `https://host/gitea` instance) are detected, but repository pages such as
/// `/gitea/owner/repo/src/branch/main/user/login` are not.
fn is_login_url(base_url: &str, url: &Url) -> bool {
    path_is_or_under(instance_relative_path(base_url, url), "/user/login")
}

/// Matches `/user/two_factor` (and its sub-pages such as `/user/two_factor/scratch`)
/// relative to the instance base path.
fn is_two_factor_url(base_url: &str, url: &Url) -> bool {
    path_is_or_under(instance_relative_path(base_url, url), "/user/two_factor")
}

/// Returns `url`'s path relative to the base path of `base_url`
/// (`/gitea/user/login` -> `/user/login` for `https://host/gitea`). URLs outside
/// the base path are returned unchanged so a redirect to a root-level login page
/// is still recognized.
fn instance_relative_path<'a>(base_url: &str, url: &'a Url) -> &'a str {
    let path = url.path();
    let base_path = Url::parse(base_url)
        .map(|base| base.path().trim_end_matches('/').to_string())
        .unwrap_or_default();
    if base_path.is_empty() {
        return path;
    }

    match path.strip_prefix(base_path.as_str()) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => path,
    }
}

fn path_is_or_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_and_two_factor_urls_are_auth_challenges() {
        let base = "https://forge.example.com";
        let login = Url::parse("https://forge.example.com/user/login?redirect_to=%2F").unwrap();
        let two_factor = Url::parse("https://forge.example.com/user/two_factor").unwrap();
        let scratch = Url::parse("https://forge.example.com/user/two_factor/scratch").unwrap();
        let settings = Url::parse("https://forge.example.com/user/settings").unwrap();

        assert!(is_auth_challenge_url(base, &login));
        assert!(is_auth_challenge_url(base, &two_factor));
        assert!(is_auth_challenge_url(base, &scratch));
        assert!(!is_auth_challenge_url(base, &settings));
    }

    #[test]
    fn auth_challenge_detection_is_relative_to_the_base_path() {
        let base = "https://apps.example.com/gitea";
        let login = Url::parse("https://apps.example.com/gitea/user/login").unwrap();
        let two_factor = Url::parse("https://apps.example.com/gitea/user/two_factor").unwrap();
        let root_login = Url::parse("https://apps.example.com/user/login").unwrap();
        let repo_file =
            Url::parse("https://apps.example.com/gitea/owner/repo/src/branch/main/user/login")
                .unwrap();
        let repo_two_factor =
            Url::parse("https://apps.example.com/gitea/owner/repo/user/two_factor").unwrap();
        let lookalike = Url::parse("https://apps.example.com/gitea/user/loginx").unwrap();

        assert!(is_login_url(base, &login));
        assert!(is_two_factor_url(base, &two_factor));
        assert!(is_login_url(base, &root_login));
        assert!(!is_auth_challenge_url(base, &repo_file));
        assert!(!is_auth_challenge_url(base, &repo_two_factor));
        assert!(!is_auth_challenge_url(base, &lookalike));
    }

    #[test]
    fn off_instance_urls_are_detected() {
        let url = |s: &str| Url::parse(s).unwrap();
        let base = "https://forge.example.com";
        assert!(!is_off_instance_url(
            base,
            &url("https://forge.example.com/user/settings")
        ));
        assert!(is_off_instance_url(
            base,
            &url("https://idp.example.com/login?client_id=forge")
        ));
        assert!(is_off_instance_url(
            base,
            &url("https://forge.example.com:9091/oauth2/sign_in")
        ));
        assert!(is_off_instance_url(
            base,
            &url("http://forge.example.com/user/settings")
        ));
        assert!(!is_off_instance_url(
            "http://forge.example.com",
            &url("https://forge.example.com/user/settings")
        ));
        assert!(!is_off_instance_url(
            "http://localhost:3000",
            &url("http://localhost:3000/user/settings")
        ));
        assert!(is_off_instance_url(
            "http://localhost:3000",
            &url("http://localhost:3001/user/settings")
        ));

        let sub = "https://apps.example.com/gitea";
        assert!(!is_off_instance_url(
            sub,
            &url("https://apps.example.com/gitea/user/settings")
        ));
        assert!(is_off_instance_url(
            sub,
            &url("https://apps.example.com/oauth2/sign_in")
        ));
        assert!(is_off_instance_url(
            sub,
            &url("https://apps.example.com/gitea2/user/settings")
        ));
    }

    /// Answers `/user/settings` with a redirect to `redirect_to(port)` and every
    /// other path with a 200 sign-in page.
    fn spawn_redirecting_server(redirect_to: impl Fn(u16) -> String + Send + 'static) -> u16 {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line == "\r\n" => break,
                        Ok(_) => {}
                    }
                }
                let response = if request_line.contains(" /user/settings ") {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        redirect_to(port)
                    )
                } else {
                    let body = "<html>Sign in with your identity provider</html>";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        port
    }

    #[tokio::test]
    async fn session_probe_rejects_a_redirect_to_an_off_instance_login_page() {
        // 127.0.0.1 redirects to `localhost`: another host, like an external IdP.
        let port = spawn_redirecting_server(|port| format!("http://localhost:{port}/idp/login"));
        let session = UiSession::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        assert!(!session.test_session().await.unwrap());
        assert_eq!(session.fetch_login().await, None);
    }

    #[tokio::test]
    async fn session_probe_names_the_canonical_host_after_a_root_url_redirect() {
        // 127.0.0.1 redirects to the same instance under its ROOT_URL host.
        let port = spawn_redirecting_server(|port| format!("http://localhost:{port}/user/login"));
        let session = UiSession::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        let err = session.test_session().await.unwrap_err().to_string();
        assert!(
            err.contains(&format!("--host http://localhost:{port}")),
            "{err}"
        );
    }

    #[test]
    fn canonical_host_redirects_keep_the_base_path_and_skip_idp_pages() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            canonical_host_redirect(
                "https://10.0.0.5/gitea",
                &url("https://git.example.com/gitea/user/login?redirect_to=x")
            )
            .as_deref(),
            Some("https://git.example.com/gitea")
        );
        assert_eq!(
            canonical_host_redirect(
                "https://forge.example.com",
                &url("https://idp.example.com/login")
            ),
            None
        );
        assert_eq!(
            canonical_host_redirect(
                "https://forge.example.com",
                &url("https://forge.example.com/user/login")
            ),
            None
        );
    }

    #[tokio::test]
    async fn session_probe_accepts_on_instance_pages() {
        let port = spawn_redirecting_server(|_| "/user/settings/profile".to_string());
        let session = UiSession::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        assert!(session.test_session().await.unwrap());
    }

    #[test]
    fn csrf_cookie_is_found_for_matching_urls_only() {
        let base = Url::parse("http://localhost:3000/").unwrap();
        let mut store = CookieStore::default();
        assert_eq!(csrf_cookie_value(&store, &base), None);
        store
            .parse("_csrf=tok123; Path=/", &base)
            .expect("cookie parses");
        store.parse("i_like_gitea=abc; Path=/", &base).unwrap();
        let post = Url::parse("http://localhost:3000/o/r/actions/runs/1/cancel").unwrap();
        assert_eq!(csrf_cookie_value(&store, &post).as_deref(), Some("tok123"));
        let other = Url::parse("http://other.example/o/r/actions/runs/1/cancel").unwrap();
        assert_eq!(csrf_cookie_value(&store, &other), None);
    }

    #[test]
    fn csrf_cookie_prefers_the_most_specific_path() {
        let base = Url::parse("https://apps.example.com/gitea/").unwrap();
        let post = Url::parse("https://apps.example.com/gitea/o/r/actions/runs/1/cancel").unwrap();
        // Insert in both orders so the result does not depend on map ordering.
        for cookies in [
            ["_csrf=stale; Path=/", "_csrf=fresh; Path=/gitea"],
            ["_csrf=fresh; Path=/gitea", "_csrf=stale; Path=/"],
        ] {
            let mut store = CookieStore::default();
            for cookie in cookies {
                store.parse(cookie, &base).expect("cookie parses");
            }
            assert_eq!(csrf_cookie_value(&store, &post).as_deref(), Some("fresh"));
        }
    }
}
