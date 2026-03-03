use std::collections::BTreeMap;
use std::time::Duration;

use eyre::{eyre, Context};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    client: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: &str, token: &str) -> eyre::Result<Self> {
        let base_url = crate::target::normalize_base_url(base_url)?;
        let base_url = base_url.trim_end_matches('/').to_string();

        let mut headers = HeaderMap::new();
        let auth_value = HeaderValue::from_str(&format!("token {token}"))
            .wrap_err("invalid api token for Authorization header")?;
        headers.insert(AUTHORIZATION, auth_value);

        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .default_headers(headers)
            .build()
            .wrap_err("failed to build http client")?;

        Ok(Self { base_url, client })
    }

    pub fn api_v1_url(&self, path: &str) -> String {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        format!("{}/api/v1{path}", self.base_url)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> eyre::Result<T> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .wrap_err_with(|| format!("GET {url} failed"))?;

        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .wrap_err_with(|| format!("failed to read response body from GET {url}"))?;

        if !status.is_success() {
            let preview = String::from_utf8_lossy(&body)
                .chars()
                .take(4096)
                .collect::<String>();
            return Err(eyre!("GET {url} failed: HTTP {status} body={preview}"));
        }

        serde_json::from_slice::<T>(&body).wrap_err_with(|| {
            let preview = String::from_utf8_lossy(&body)
                .chars()
                .take(4096)
                .collect::<String>();
            format!("failed to decode JSON from GET {url}: body={preview}")
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RegistrationToken {
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ActionRunJob {
    pub id: i64,

    #[serde(default)]
    pub name: String,

    pub status: Option<String>,

    #[serde(rename = "runs_on", default)]
    pub runs_on: Vec<String>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ActionRunJob {
    pub fn runs_on_display(&self) -> String {
        self.runs_on.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_token_deserializes() {
        let raw = r#"{ "token": "abc" }"#;
        let tok: RegistrationToken = serde_json::from_str(raw).unwrap();
        assert_eq!(tok.token, "abc");
    }

    #[test]
    fn runner_jobs_deserialize_minimal_list() {
        let raw = r#"
[
  {
    "id": 1,
    "status": "waiting",
    "name": "build",
    "runs_on": ["self-hosted", "linux"]
  }
]
"#;

        let jobs: Vec<ActionRunJob> = serde_json::from_str(raw).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, 1);
        assert_eq!(jobs[0].status.as_deref(), Some("waiting"));
        assert_eq!(jobs[0].name, "build");
        assert_eq!(jobs[0].runs_on, vec!["self-hosted", "linux"]);
    }
}
