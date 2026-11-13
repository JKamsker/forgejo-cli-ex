use std::collections::BTreeMap;
use std::path::Path;
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
    pub fn new_with_socket(
        base_url: &str,
        token: &str,
        unix_socket: Option<&Path>,
    ) -> eyre::Result<Self> {
        let mut auth_value = HeaderValue::from_str(&format!("token {token}"))
            .wrap_err("invalid api token for Authorization header")?;
        auth_value.set_sensitive(true);
        Self::build(base_url, Some(auth_value), unix_socket)
    }

    /// Client that authenticates every request with `auth` (token or basic).
    pub fn from_auth(
        base_url: &str,
        auth: &crate::credentials::ApiAuth,
        unix_socket: Option<&Path>,
    ) -> eyre::Result<Self> {
        Self::build(base_url, Some(auth.header_value()?), unix_socket)
    }

    fn build(
        base_url: &str,
        auth_value: Option<HeaderValue>,
        unix_socket: Option<&Path>,
    ) -> eyre::Result<Self> {
        let base_url = crate::target::normalize_base_url(base_url)?;
        let base_url = base_url.trim_end_matches('/').to_string();

        let mut headers = HeaderMap::new();
        if let Some(auth_value) = auth_value {
            headers.insert(AUTHORIZATION, auth_value);
        }

        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .default_headers(headers);

        #[cfg(unix)]
        if let Some(socket_path) = unix_socket {
            builder = builder.unix_socket(socket_path);
        }
        #[cfg(not(unix))]
        let _ = unix_socket;

        let client = builder.build().wrap_err("failed to build http client")?;

        Ok(Self { base_url, client })
    }

    /// The underlying HTTP client (requests carry the configured credential).
    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn api_v1_url(&self, path: &str) -> String {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };

        // Convert http+unix:// URLs to http://localhost for HTTP requests
        // (the Unix socket transport is configured separately via builder.unix_socket)
        let request_base = if self.base_url.starts_with("http+unix://") {
            "http://localhost".to_string()
        } else {
            self.base_url.clone()
        };

        format!("{}/api/v1{path}", request_base)
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
            return Err(eyre!(
                "GET {url} failed: HTTP {status} (body_length={})",
                body.len()
            ));
        }

        serde_json::from_slice::<T>(&body).wrap_err_with(|| {
            format!(
                "failed to decode JSON from GET {url} (body_length={})",
                body.len()
            )
        })
    }

    /// Fetch a runner registration token from the first of `urls` that serves one.
    ///
    /// Forgejo and Gitea <= 1.26 answer `GET`; Gitea 1.27 removed the `GET`
    /// routes in favour of `POST`. A 404/405 moves on to the next method/URL.
    pub async fn registration_token(&self, urls: &[String]) -> eyre::Result<RegistrationToken> {
        let mut attempts = Vec::new();
        for url in urls {
            for method in [reqwest::Method::GET, reqwest::Method::POST] {
                let resp = self
                    .client
                    .request(method.clone(), url)
                    .send()
                    .await
                    .wrap_err_with(|| format!("{method} {url} failed"))?;
                let status = resp.status();
                if registration_token_route_missing(status) {
                    attempts.push(format!("{method} {url}: HTTP {status}"));
                    continue;
                }
                let body = resp.bytes().await.wrap_err_with(|| {
                    format!("failed to read response body from {method} {url}")
                })?;
                if !status.is_success() {
                    return Err(eyre!(
                        "{method} {url} failed: HTTP {status} (body_length={})",
                        body.len()
                    ));
                }
                return serde_json::from_slice(&body).wrap_err_with(|| {
                    format!(
                        "failed to decode JSON from {method} {url} (body_length={})",
                        body.len()
                    )
                });
            }
        }
        Err(eyre!(
            "No runner registration token endpoint found:\n - {}",
            attempts.join("\n - ")
        ))
    }

    /// Runner job queue.
    ///
    /// Forgejo serves `.../actions/runners/jobs` (and answers `null` for an
    /// empty queue): the waiting jobs a runner with `labels` could pick up.
    /// Gitea has no such route (404) but lists jobs at `.../actions/jobs` in
    /// a GitHub-compatible shape; there the queued jobs are read page by page,
    /// mapped to Forgejo's vocabulary (`queued` -> `waiting`, `labels` ->
    /// `runs_on`), and `labels` is applied client-side with Forgejo's rule.
    pub async fn runner_jobs(
        &self,
        url: &str,
        base_jobs_url: &str,
        labels: &[String],
    ) -> eyre::Result<Vec<ActionRunJob>> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .wrap_err_with(|| format!("GET {url} failed"))?;
        let gitea_url = gitea_jobs_url(base_jobs_url);
        if resp.status() != reqwest::StatusCode::NOT_FOUND || gitea_url.is_none() {
            let status = resp.status();
            let body = resp
                .bytes()
                .await
                .wrap_err_with(|| format!("failed to read response body from GET {url}"))?;
            if !status.is_success() {
                return Err(eyre!(
                    "GET {url} failed: HTTP {status} (body_length={})",
                    body.len()
                ));
            }
            let jobs: Option<Vec<ActionRunJob>> = serde_json::from_slice(&body)
                .wrap_err_with(|| format!("failed to decode JSON from GET {url}"))?;
            return Ok(jobs.unwrap_or_default());
        }

        let gitea_url = gitea_url.unwrap_or_default();
        let mut jobs = Vec::new();
        // The server may cap the page size below what was asked for
        // (MAX_RESPONSE_ITEMS), so a short page alone does not prove the
        // listing is complete; `total_count` does when the server sends it.
        let mut read = 0_u64;
        let mut total = None;
        for page in 1..=GITEA_QUEUE_MAX_PAGES {
            let url =
                format!("{gitea_url}?status=queued&page={page}&limit={GITEA_QUEUE_PAGE_SIZE}");
            let listed: GiteaWorkflowJobs = self.get_json(&url).await?;
            let count = listed.jobs.len();
            read += count as u64;
            total = listed.total_count.or(total);
            jobs.extend(
                listed
                    .jobs
                    .into_iter()
                    .map(GiteaWorkflowJob::into_runner_job)
                    .filter(|job| job.status.as_deref() == Some("waiting"))
                    .filter(|job| runner_could_take(job, labels)),
            );
            let done = match total {
                Some(total) => read >= total,
                None => count < GITEA_QUEUE_PAGE_SIZE,
            };
            if done || count == 0 {
                return Ok(jobs);
            }
        }
        if total.is_some_and(|total| read < total) {
            eprintln!(
                "warn: listed only the first {read} of {} queued jobs (page limit reached); the result may be incomplete.",
                total.unwrap_or_default()
            );
        }
        Ok(jobs)
    }

    pub async fn get_json_with_basic_auth<T: DeserializeOwned>(
        &self,
        url: &str,
        username: &str,
        password: &str,
    ) -> eyre::Result<T> {
        let resp = self
            .client
            .get(url)
            .basic_auth(username, Some(password))
            .send()
            .await
            .wrap_err_with(|| format!("GET {url} failed"))?;

        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .wrap_err_with(|| format!("failed to read response body from GET {url}"))?;

        if !status.is_success() {
            return Err(eyre!(
                "GET {url} failed: HTTP {status} (body_length={})",
                body.len()
            ));
        }

        serde_json::from_slice::<T>(&body).wrap_err_with(|| {
            format!(
                "failed to decode JSON from GET {url} (body_length={})",
                body.len()
            )
        })
    }

    pub async fn post_json_with_basic_auth<T: DeserializeOwned, B: Serialize>(
        &self,
        url: &str,
        body: &B,
        username: &str,
        password: &str,
    ) -> eyre::Result<T> {
        let resp = self
            .client
            .post(url)
            .basic_auth(username, Some(password))
            .json(body)
            .send()
            .await
            .wrap_err_with(|| format!("POST {url} failed"))?;

        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .wrap_err_with(|| format!("failed to read response body from POST {url}"))?;

        if !status.is_success() {
            return Err(eyre!(
                "POST {url} failed: HTTP {status} (body_length={})",
                body.len()
            ));
        }

        serde_json::from_slice::<T>(&body).wrap_err_with(|| {
            format!(
                "failed to decode JSON from POST {url} (body_length={})",
                body.len()
            )
        })
    }
}

/// Gitea's equivalent of a Forgejo `.../runners/jobs` URL.
fn gitea_jobs_url(forgejo_jobs_url: &str) -> Option<String> {
    let base = forgejo_jobs_url
        .split('?')
        .next()
        .unwrap_or(forgejo_jobs_url);
    if let Some(prefix) = base.strip_suffix("/admin/runners/jobs") {
        return Some(format!("{prefix}/admin/actions/jobs"));
    }
    base.strip_suffix("/actions/runners/jobs")
        .map(|prefix| format!("{prefix}/actions/jobs"))
}

/// Gitea serves at most 50 items per API page by default.
const GITEA_QUEUE_PAGE_SIZE: usize = 50;
/// Upper bound on queue pages read from Gitea by one `runners jobs` call.
const GITEA_QUEUE_MAX_PAGES: usize = 20;

/// Forgejo's queue rule: with labels given, a job is listed only when a
/// runner with exactly those labels could run it, i.e. every `runs_on` label
/// is among them.
fn runner_could_take(job: &ActionRunJob, labels: &[String]) -> bool {
    labels.is_empty() || (!job.runs_on.is_empty() && job.runs_on.iter().all(|l| labels.contains(l)))
}

/// One item of Gitea's `GET .../actions/jobs` (GitHub-compatible shape).
#[derive(Deserialize, Debug)]
struct GiteaWorkflowJob {
    id: i64,
    #[serde(default)]
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize, Debug)]
struct GiteaWorkflowJobs {
    #[serde(default)]
    jobs: Vec<GiteaWorkflowJob>,
    #[serde(default)]
    total_count: Option<u64>,
}

impl GiteaWorkflowJob {
    fn into_runner_job(self) -> ActionRunJob {
        let status = match self.status.as_deref() {
            Some("queued") | Some("waiting") | Some("pending") => Some("waiting".to_string()),
            Some("in_progress") => Some("running".to_string()),
            Some("completed") => self.conclusion.clone().or(self.status.clone()),
            _ => self.status.clone(),
        };
        let mut extra = self.extra;
        if let Some(conclusion) = self.conclusion {
            extra.insert("conclusion".to_string(), Value::String(conclusion));
        }
        ActionRunJob {
            id: self.id,
            name: self.name,
            status,
            runs_on: self.labels,
            extra,
        }
    }
}

fn registration_token_route_missing(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
    )
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RegistrationToken {
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreatedAccessToken {
    pub id: i64,
    pub name: String,
    #[serde(rename = "sha1")]
    pub token: String,
    #[serde(rename = "token_last_eight")]
    pub token_last_eight: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ListedAccessToken {
    pub id: i64,
    pub name: String,
    #[serde(rename = "token_last_eight")]
    pub token_last_eight: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthenticatedUser {
    pub login: String,
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
    fn gitea_jobs_urls_mirror_forgejo_runner_job_urls() {
        assert_eq!(
            gitea_jobs_url("http://h/api/v1/repos/o/r/actions/runners/jobs").as_deref(),
            Some("http://h/api/v1/repos/o/r/actions/jobs")
        );
        assert_eq!(
            gitea_jobs_url("http://h/api/v1/orgs/o/actions/runners/jobs?labels=a").as_deref(),
            Some("http://h/api/v1/orgs/o/actions/jobs")
        );
        assert_eq!(
            gitea_jobs_url("http://h/api/v1/user/actions/runners/jobs").as_deref(),
            Some("http://h/api/v1/user/actions/jobs")
        );
        assert_eq!(
            gitea_jobs_url("http://h/api/v1/admin/runners/jobs").as_deref(),
            Some("http://h/api/v1/admin/actions/jobs")
        );
        assert_eq!(gitea_jobs_url("http://h/api/v1/other"), None);
    }

    #[test]
    fn gitea_workflow_jobs_map_to_runner_jobs() {
        let raw = r#"{"total_count": 2, "jobs": [
            {"id": 4, "name": "queued", "status": "queued", "conclusion": null,
             "labels": ["fjex-no-such-runner"], "run_id": 3},
            {"id": 5, "name": "build", "status": "completed", "conclusion": "success",
             "labels": ["ubuntu-latest"]}
        ]}"#;
        let listed: GiteaWorkflowJobs = serde_json::from_str(raw).unwrap();
        let jobs: Vec<ActionRunJob> = listed
            .jobs
            .into_iter()
            .map(GiteaWorkflowJob::into_runner_job)
            .collect();
        assert_eq!(jobs[0].status.as_deref(), Some("waiting"));
        assert_eq!(jobs[0].runs_on, vec!["fjex-no-such-runner"]);
        assert_eq!(jobs[0].extra.get("run_id"), Some(&serde_json::json!(3)));
        assert_eq!(jobs[1].status.as_deref(), Some("success"));
    }

    #[test]
    fn gitea_queue_labels_follow_forgejo_subset_rule() {
        let job = |runs_on: &[&str]| ActionRunJob {
            id: 1,
            name: "queued".to_string(),
            status: Some("waiting".to_string()),
            runs_on: runs_on.iter().map(|l| l.to_string()).collect(),
            extra: BTreeMap::new(),
        };
        let linux = vec!["linux".to_string()];
        assert!(runner_could_take(&job(&["linux"]), &linux));
        assert!(!runner_could_take(&job(&["linux", "gpu"]), &linux));
        assert!(!runner_could_take(&job(&[]), &linux));
        assert!(runner_could_take(
            &job(&["linux"]),
            &["linux".to_string(), "gpu".to_string()]
        ));
        assert!(runner_could_take(&job(&["linux", "gpu"]), &[]));
    }

    #[test]
    fn registration_token_falls_through_only_for_missing_routes() {
        use reqwest::StatusCode;
        assert!(registration_token_route_missing(StatusCode::NOT_FOUND));
        assert!(registration_token_route_missing(
            StatusCode::METHOD_NOT_ALLOWED
        ));
        assert!(!registration_token_route_missing(StatusCode::FORBIDDEN));
        assert!(!registration_token_route_missing(StatusCode::OK));
    }

    #[test]
    fn registration_token_deserializes() {
        let raw = r#"{ "token": "abc" }"#;
        let tok: RegistrationToken = serde_json::from_str(raw).unwrap();
        assert_eq!(tok.token, "abc");
    }

    #[test]
    fn created_access_token_deserializes() {
        let raw = r#"
{
  "id": 1,
  "name": "fj-ex-nuget",
  "sha1": "abc123",
  "token_last_eight": "bc123",
  "scopes": ["write:package"]
}
"#;
        let tok: CreatedAccessToken = serde_json::from_str(raw).unwrap();
        assert_eq!(tok.id, 1);
        assert_eq!(tok.name, "fj-ex-nuget");
        assert_eq!(tok.token, "abc123");
        assert_eq!(tok.token_last_eight, "bc123");
        assert_eq!(tok.scopes, vec!["write:package"]);
    }

    #[test]
    fn listed_access_token_deserializes() {
        let raw = r#"
[
  {
    "id": 1,
    "name": "fj-ex-nuget",
    "token_last_eight": "bc123",
    "scopes": ["write:package"]
  }
]
"#;
        let toks: Vec<ListedAccessToken> = serde_json::from_str(raw).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].id, 1);
        assert_eq!(toks[0].name, "fj-ex-nuget");
        assert_eq!(toks[0].token_last_eight, "bc123");
        assert_eq!(toks[0].scopes, vec!["write:package"]);
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
