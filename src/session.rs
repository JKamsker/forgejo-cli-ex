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
        let cookie_jar = if force_relogin {
            None
        } else {
            info.entry.and_then(|e| e.cookie_jar)
        };

        let session = Self::new_with_socket(&normalized, cookie_jar.as_ref(), effective_socket)?;
        if !force_relogin {
            if session.test_session().await.unwrap_or(false) {
                let _ = session.persist_cookie_jar().await;
                return Ok(session);
            }
        }

        let creds = store::get_ui_creds(&normalized).await?.ok_or_else(|| {
            eyre!(
                "No stored UI creds for '{}'. Run `fj-ex auth login` (or legacy `fj-ex login`) first.",
                normalized
            )
        })?;

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

        if is_auth_challenge_url(resp.url()) {
            return Ok(false);
        }
        if resp.status().as_u16() >= 400 {
            return Ok(false);
        }
        Ok(true)
    }

    pub async fn relogin_from_store(&self) -> eyre::Result<()> {
        store::clear_cookie_jar(self.storage_base_url()).await?;
        {
            let mut guard = self.cookie_store.lock().unwrap();
            guard.clear();
        }

        let creds = store::get_ui_creds(self.storage_base_url()).await?.ok_or_else(|| {
            eyre!(
                "No stored UI creds for '{}'. Run `fj-ex auth login` (or legacy `fj-ex login`) first.",
                self.storage_base_url()
            )
        })?;
        self.login_with_creds(&creds.username, &creds.password)
            .await?;
        self.persist_cookie_jar_required().await?;
        Ok(())
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

        if is_two_factor_url(login_resp.url()) {
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

        if is_two_factor_url(resp.url()) {
            return Err(eyre!(
                "Two-factor passcode was rejected for '{}'.",
                self.base_url
            ));
        }
        if is_login_url(resp.url()) {
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
        self.send_with_retry(retry_on_logout, || self.client.post(url).json(body))
            .await
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
        if retry_on_logout && is_auth_challenge_url(resp.url()) {
            self.relogin_from_store().await?;
            let resp = build().send().await.wrap_err("request failed")?;
            self.persist_cookie_jar_required().await?;
            return Ok(resp);
        }

        self.persist_cookie_jar().await?;
        Ok(resp)
    }
}

fn is_auth_challenge_url(url: &Url) -> bool {
    is_login_url(url) || is_two_factor_url(url)
}

fn is_login_url(url: &Url) -> bool {
    url.path().starts_with("/user/login")
}

fn is_two_factor_url(url: &Url) -> bool {
    url.path().starts_with("/user/two_factor")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_and_two_factor_urls_are_auth_challenges() {
        let login = Url::parse("https://forge.example.com/user/login").unwrap();
        let two_factor = Url::parse("https://forge.example.com/user/two_factor").unwrap();
        let settings = Url::parse("https://forge.example.com/user/settings").unwrap();

        assert!(is_auth_challenge_url(&login));
        assert!(is_auth_challenge_url(&two_factor));
        assert!(!is_auth_challenge_url(&settings));
    }
}
