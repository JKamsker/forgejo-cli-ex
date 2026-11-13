//! Forgejo/Gitea REST API helpers used for seeding (independent of `fj-ex`,
//! so seeding never depends on the code under test).

use std::time::Duration;

use eyre::{eyre, Context, Result};
use serde_json::{json, Value};

/// How a request authenticates.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    Basic {
        username: &'a str,
        password: &'a str,
    },
    Token(&'a str),
    None,
}

#[derive(Clone)]
pub struct Api {
    base_url: String,
    client: reqwest::Client,
}

impl Api {
    pub fn new(base_url: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .wrap_err("failed to build http client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url, path)
    }

    /// Send a request and return `(status, body)` without judging the status.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        auth: Auth<'_>,
        body: Option<&Value>,
    ) -> Result<(reqwest::StatusCode, String)> {
        let mut req = self.client.request(method.clone(), self.url(path));
        req = match auth {
            Auth::Basic { username, password } => req.basic_auth(username, Some(password)),
            Auth::Token(token) => req.header("Authorization", format!("token {token}")),
            Auth::None => req,
        };
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .wrap_err_with(|| format!("{method} {path} failed"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// Send a request that must succeed (2xx); returns the parsed JSON body (or `Null`).
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        auth: Auth<'_>,
        body: Option<&Value>,
    ) -> Result<Value> {
        let (status, text) = self.send(method.clone(), path, auth, body).await?;
        if !status.is_success() {
            return Err(eyre!("{method} {path}: HTTP {status}: {}", clip(&text)));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).wrap_err_with(|| format!("{method} {path}: invalid JSON"))
    }

    pub async fn get(&self, path: &str, auth: Auth<'_>) -> Result<Value> {
        self.call(reqwest::Method::GET, path, auth, None).await
    }

    pub async fn post(&self, path: &str, auth: Auth<'_>, body: &Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, auth, Some(body))
            .await
    }

    /// `GET /api/v1/version` (unauthenticated); `None` until the server answers.
    pub async fn version(&self) -> Option<String> {
        let (status, text) = self
            .send(reqwest::Method::GET, "/version", Auth::None, None)
            .await
            .ok()?;
        if !status.is_success() {
            return None;
        }
        serde_json::from_str::<Value>(&text)
            .ok()?
            .get("version")?
            .as_str()
            .map(str::to_string)
    }

    /// Create a personal access token for `username` (Basic auth with the
    /// account password) and return its secret.
    ///
    /// `scopes` uses the server's scope names, e.g. `["all"]` or
    /// `["write:repository", "read:user"]`. Useful for token-auth scenarios.
    pub async fn create_token(
        &self,
        username: &str,
        password: &str,
        token_name: &str,
        scopes: &[&str],
    ) -> Result<String> {
        let body = json!({ "name": token_name, "scopes": scopes });
        let created = self
            .post(
                &format!("/users/{}/tokens", urlencoding::encode(username)),
                Auth::Basic { username, password },
                &body,
            )
            .await
            .wrap_err("failed to create access token")?;
        created
            .get("sha1")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| eyre!("token response had no sha1 field"))
    }

    /// Create a regular (non-admin) user via the admin API.
    pub async fn create_user(
        &self,
        admin_token: &str,
        username: &str,
        password: &str,
        email: &str,
    ) -> Result<()> {
        let body = json!({
            "username": username,
            "password": password,
            "email": email,
            "must_change_password": false,
        });
        self.post("/admin/users", Auth::Token(admin_token), &body)
            .await
            .wrap_err_with(|| format!("failed to create user {username}"))?;
        Ok(())
    }

    pub async fn create_org(&self, token: &str, org: &str) -> Result<()> {
        let body = json!({ "username": org, "visibility": "public" });
        self.post("/orgs", Auth::Token(token), &body)
            .await
            .wrap_err_with(|| format!("failed to create org {org}"))?;
        Ok(())
    }

    /// Create an empty repository owned by the token's user (`org = None`) or an org.
    pub async fn create_repo(
        &self,
        token: &str,
        org: Option<&str>,
        name: &str,
        description: &str,
    ) -> Result<Value> {
        let body = json!({
            "name": name,
            "description": description,
            "private": false,
            "auto_init": false,
            "default_branch": "main",
        });
        let path = match org {
            Some(org) => format!("/orgs/{}/repos", urlencoding::encode(org)),
            None => "/user/repos".to_string(),
        };
        self.post(&path, Auth::Token(token), &body)
            .await
            .wrap_err_with(|| format!("failed to create repo {name}"))
    }

    /// Commit SHA at the tip of `branch`.
    pub async fn branch_sha(&self, token: &str, repo: &str, branch: &str) -> Result<String> {
        let v = self
            .get(
                &format!("/repos/{repo}/branches/{}", urlencoding::encode(branch)),
                Auth::Token(token),
            )
            .await?;
        v.pointer("/commit/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| eyre!("branch {branch} of {repo} has no commit id"))
    }
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 500 {
        format!("{}…", s.chars().take(500).collect::<String>())
    } else {
        s.to_string()
    }
}
