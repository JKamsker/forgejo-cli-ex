//! Minimal web-UI client for setup steps that have no REST API (2FA enrollment).
//! Also the starting point for future session-cookie-import scenarios.

use std::time::Duration;

use eyre::{eyre, Context, Result};
use regex::Regex;

pub struct WebSession {
    base_url: String,
    client: reqwest::Client,
}

fn csrf_from_html(html: &str) -> Option<String> {
    let patterns = [
        r#"name="_csrf"\s+value="([^"]+)""#,
        r#"value="([^"]+)"\s+name="_csrf""#,
        r#"<meta\s+name="_csrf"\s+content="([^"]+)""#,
        r#"csrfToken:\s*'([^']+)'"#,
    ];
    patterns.iter().find_map(|p| {
        Regex::new(p)
            .ok()?
            .captures(html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().replace("&#43;", "+").replace("&amp;", "&"))
    })
}

impl WebSession {
    /// Log in through `/user/login` and keep the session cookies.
    pub async fn login(base_url: &str, username: &str, password: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .build()?;
        let session = Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        };
        let page = session.get_text("/user/login").await?;
        let mut form = vec![
            ("user_name", username.to_string()),
            ("password", password.to_string()),
        ];
        if let Some(csrf) = csrf_from_html(&page) {
            form.push(("_csrf", csrf));
        }
        let resp = session
            .client
            .post(format!("{}/user/login", session.base_url))
            .form(&form)
            .send()
            .await
            .wrap_err("login request failed")?;
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !resp.status().is_redirection() || location.contains("/user/login") {
            return Err(eyre!(
                "web login as {username} failed: HTTP {} location={location:?}",
                resp.status()
            ));
        }
        Ok(session)
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
            .wrap_err_with(|| format!("GET {path} failed"))?;
        if !resp.status().is_success() {
            return Err(eyre!("GET {path}: HTTP {}", resp.status()));
        }
        Ok(resp.text().await?)
    }

    /// Enroll TOTP two-factor authentication for the logged-in user and
    /// return the base32 secret.
    pub async fn enroll_totp(&self) -> Result<String> {
        const ENROLL: &str = "/user/settings/security/two_factor/enroll";
        let page = self.get_text(ENROLL).await?;
        let secret = Regex::new(r"enter the secret:\s*([A-Z2-7]{16,})")?
            .captures(&page)
            .or_else(|| Regex::new(r"\b([A-Z2-7]{32,})\b").ok()?.captures(&page))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| eyre!("could not find the TOTP secret on {ENROLL}"))?;
        let passcode = super::totp::totp_now(&secret, 0)?;
        let mut form = vec![("passcode", passcode)];
        if let Some(csrf) = csrf_from_html(&page) {
            form.push(("_csrf", csrf));
        }
        let resp = self
            .client
            .post(format!("{}{ENROLL}", self.base_url))
            .form(&form)
            .send()
            .await
            .wrap_err("2FA enroll request failed")?;
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !resp.status().is_redirection() || location.contains("enroll") {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let flash = Regex::new(r#"(?s)class="[^"]*flash-error[^"]*"[^>]*>(.*?)</div>"#)
                .ok()
                .and_then(|r| {
                    r.captures(&body)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().trim().to_string())
                });
            return Err(eyre!(
                "2FA enrollment failed: HTTP {status} location={location:?} flash={flash:?}"
            ));
        }
        Ok(secret)
    }
}
