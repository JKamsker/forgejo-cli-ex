use std::{path::Path, sync::Arc, time::Duration};

use cookie_store::{Cookie, CookieExpiration, CookieStore};
use eyre::{eyre, Context};
use reqwest::redirect::Policy;
use reqwest_cookie_store::CookieStoreMutex;
use time::OffsetDateTime;
use url::Url;

use crate::{html, store};

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct UiSession {
    base_url: String,
    cookie_store: Arc<CookieStoreMutex>,
    client: reqwest::Client,
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
        session.persist_cookie_jar().await?;
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
            load_cookie_jar_into_store(&cookie_store, jar)?;
        }

        let cookie_store = Arc::new(cookie_store);
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::limited(10))
            .timeout(Duration::from_secs(60))
            .cookie_provider(Arc::clone(&cookie_store));

        #[cfg(unix)]
        if let Some(socket_path) = effective_socket {
            builder = builder.unix_socket(socket_path);
        }

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

        if is_logged_out_url(resp.url()) {
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
        self.persist_cookie_jar().await?;
        Ok(())
    }

    pub async fn login_with_creds(&self, username: &str, password: &str) -> eyre::Result<()> {
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

        self.client
            .post(&login_url)
            .form(&form)
            .send()
            .await
            .wrap_err("failed to post login form")?;

        let ok = self.test_session().await?;
        if !ok {
            return Err(eyre!(
                "Login failed for '{}' on '{}' (landed on /user/login while validating).",
                username,
                self.base_url
            ));
        }

        Ok(())
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
        let jar = cookie_jar_from_store(&self.cookie_store)?;
        store::save_cookie_jar(self.storage_base_url(), jar).await?;
        Ok(())
    }

    async fn send_with_retry(
        &self,
        retry_on_logout: bool,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> eyre::Result<reqwest::Response> {
        let resp = build().send().await.wrap_err("request failed")?;
        if retry_on_logout && is_logged_out_url(resp.url()) {
            self.relogin_from_store().await?;
            let resp = build().send().await.wrap_err("request failed")?;
            self.persist_cookie_jar().await?;
            return Ok(resp);
        }

        self.persist_cookie_jar().await?;
        Ok(resp)
    }
}

fn is_logged_out_url(url: &Url) -> bool {
    url.path().starts_with("/user/login")
}

fn load_cookie_jar_into_store(
    cookie_store: &CookieStoreMutex,
    jar: &store::CookieJar,
) -> eyre::Result<()> {
    let mut guard = cookie_store.lock().unwrap();

    for record in &jar.cookies {
        let domain = record.domain.trim().trim_start_matches('.');
        if domain.is_empty() {
            continue;
        }

        let request_url = match Url::parse(&format!("https://{domain}/")) {
            Ok(u) => u,
            Err(_) => continue,
        };

        let mut builder =
            cookie_store::RawCookie::build((record.name.clone(), record.value.clone()))
                .domain(domain.to_string())
                .path(record.path.clone())
                .secure(record.secure)
                .http_only(record.http_only);

        if let Some(same_site) = record.same_site.as_deref() {
            let same_site = match same_site.to_ascii_lowercase().as_str() {
                "lax" => Some(cookie::SameSite::Lax),
                "strict" => Some(cookie::SameSite::Strict),
                "none" => Some(cookie::SameSite::None),
                _ => None,
            };
            if let Some(same_site) = same_site {
                builder = builder.same_site(same_site);
            }
        }

        if let Some(expires) = record.expires_utc.as_deref() {
            if let Ok(dt) =
                time::OffsetDateTime::parse(expires, &time::format_description::well_known::Rfc3339)
            {
                builder = builder.expires(dt);
            }
        }

        let raw_cookie = builder.build();
        let cookie = Cookie::try_from_raw_cookie(&raw_cookie, &request_url)
            .wrap_err("failed to reconstruct cookie")?
            .into_owned();
        let _ = guard.insert(cookie, &request_url);
    }

    Ok(())
}

fn cookie_jar_from_store(cookie_store: &CookieStoreMutex) -> eyre::Result<store::CookieJar> {
    let guard = cookie_store.lock().unwrap();
    let now = OffsetDateTime::now_utc();

    let mut cookies = Vec::new();
    for c in guard.iter_any() {
        if c.is_expired() {
            continue;
        }

        let domain = c.domain.as_cow().map(|s| s.to_string()).unwrap_or_default();
        if domain.is_empty() {
            continue;
        }

        let expires_utc = match c.expires {
            CookieExpiration::AtUtc(dt) => Some(
                dt.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            ),
            CookieExpiration::SessionEnd => None,
        };

        cookies.push(store::CookieRecord {
            name: c.name().to_string(),
            value: c.value().to_string(),
            domain,
            path: c.path.to_string(),
            expires_utc,
            secure: c.secure().unwrap_or(false),
            http_only: c.http_only().unwrap_or(false),
            same_site: c.same_site().map(|s| s.to_string()),
        });
    }

    Ok(store::CookieJar {
        saved_utc: Some(
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        ),
        cookies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_records_roundtrip_into_cookie_store() {
        let jar = store::CookieJar {
            saved_utc: Some("2020-01-01T00:00:00Z".to_string()),
            cookies: vec![store::CookieRecord {
                name: "a".to_string(),
                value: "b".to_string(),
                domain: "forge.example.com".to_string(),
                path: "/".to_string(),
                expires_utc: None,
                secure: true,
                http_only: true,
                same_site: Some("Lax".to_string()),
            }],
        };

        let cookie_store = CookieStoreMutex::new(CookieStore::default());
        load_cookie_jar_into_store(&cookie_store, &jar).unwrap();

        let out = cookie_jar_from_store(&cookie_store).unwrap();
        assert_eq!(out.cookies.len(), 1);
        assert_eq!(out.cookies[0].name, "a");
        assert_eq!(out.cookies[0].value, "b");
        assert_eq!(out.cookies[0].domain, "forge.example.com");
        assert_eq!(out.cookies[0].path, "/");
        assert!(out.cookies[0].secure);
        assert!(out.cookies[0].http_only);
        assert_eq!(out.cookies[0].same_site.as_deref(), Some("Lax"));
    }
}
