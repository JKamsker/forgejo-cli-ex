//! REST API (`/api/v1`) backend for `fj-ex actions …`, used with API tokens.
//!
//! Forgejo/Gitea web UI routes do not accept API tokens, so token-only
//! automation needs the REST API. Coverage differs per server family and
//! version (verified against the live servers' `/swagger.v1.json`):
//!
//! | operation              | Forgejo 15 | Forgejo 16 | Forgejo 17 | Gitea 1.25 | Gitea 1.26 | Gitea 1.27 |
//! |------------------------|------------|------------|------------|------------|------------|------------|
//! | list runs / get run    | yes        | yes        | yes        | yes        | yes        | yes        |
//! | list jobs of a run     | no         | yes        | yes        | yes        | yes        | yes        |
//! | job logs               | no         | yes        | yes        | yes        | yes        | yes        |
//! | logs of older attempts | no         | yes        | yes        | no         | no         | yes        |
//! | cancel run             | no         | yes        | yes        | no         | no         | no         |
//! | rerun run / job        | no         | no         | yes        | no         | yes        | yes        |
//! | rerun failed jobs      | no         | no         | yes (*)    | no         | yes        | yes        |
//! | list/download artifacts| no         | yes        | yes        | yes (v4)   | yes (v4)   | yes (v4)   |
//! | list workflows         | no         | no         | no         | yes        | yes        | yes        |
//! | workflow dispatch      | yes        | yes        | yes        | yes        | yes        | yes        |
//!
//! (*) Forgejo has no single endpoint; each failed job is rerun individually.
//! (v4) Gitea's REST API only covers artifacts uploaded with the v4 protocol
//! (`actions/upload-artifact@v4`); v3 artifacts need the UI backend.
//!
//! The CLI addresses runs by the number shown in the run's web URL. That is
//! the per-repo run index on Forgejo and Gitea <= 1.25, but the run *id* on
//! Gitea >= 1.26. The API always takes the id, so runs are resolved through
//! their `html_url`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use eyre::{eyre, Context};
use regex::Regex;
use serde_json::Value;

use crate::ui_actions::{JobInfo, RunRef};

static RUN_URL_NUMBER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/actions/runs/(?P<n>\d+)(?:[/?#]|$)").expect("RUN_URL_NUMBER_RE must be valid")
});

/// Page size used when scanning run lists.
const SCAN_PAGE_SIZE: u32 = 50;
/// Upper bound on pages scanned to map a run number or filter by workflow.
const MAX_SCAN_PAGES: u32 = 40;

/// Decides when a paged list scan is complete.
///
/// Servers clamp `limit` to their `[api] MAX_RESPONSE_ITEMS`, so a page that
/// is shorter than requested does not by itself mean the list ended. The
/// response's `total_count` decides when there is one; otherwise a page
/// shorter than the server's apparent page size (the first page's length)
/// does, and a short first page is confirmed with one more request.
#[derive(Debug, Default)]
struct PageScan {
    first_len: Option<usize>,
    seen: usize,
}

impl PageScan {
    /// Record a page of `got` items; returns whether to fetch the next page.
    fn more(&mut self, got: usize, total: Option<i64>, requested: u32) -> bool {
        self.seen += got;
        let first_len = *self.first_len.get_or_insert(got);
        if got == 0 {
            return false;
        }
        match total {
            Some(total) => (self.seen as i64) < total,
            None if self.seen == got => true,
            None => got >= first_len.min(requested as usize),
        }
    }
}

/// `total_count` of a list response, if the server sent one.
fn total_count(v: &Value) -> Option<i64> {
    v.get("total_count").and_then(Value::as_i64)
}

pub type Version = (u64, u64, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerFamily {
    Forgejo,
    Gitea,
}

impl ServerFamily {
    pub fn name(self) -> &'static str {
        match self {
            Self::Forgejo => "Forgejo",
            Self::Gitea => "Gitea",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub family: ServerFamily,
    pub version: Option<Version>,
    pub raw_version: String,
}

impl ServerInfo {
    /// Classify a `/api/v1/version` string. Forgejo reports e.g.
    /// `17.0.1+gitea-1.22.0`; `forgejo_probe` is the result of probing
    /// `/api/forgejo/v1/version` when the string alone is not conclusive.
    pub fn from_version(raw: &str, forgejo_probe: bool) -> Self {
        let family = if raw.contains("+gitea") || forgejo_probe {
            ServerFamily::Forgejo
        } else {
            ServerFamily::Gitea
        };
        Self {
            family,
            version: parse_version(raw),
            raw_version: raw.trim().to_string(),
        }
    }

    pub fn label(&self) -> String {
        let version = self.raw_version.split('+').next().unwrap_or_default();
        if version.is_empty() {
            self.family.name().to_string()
        } else {
            format!("{} {}", self.family.name(), version)
        }
    }
}

/// Parse the leading `major.minor.patch` of a version string.
pub fn parse_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().trim_start_matches('v');
    let mut parts = raw
        .split(|c: char| !c.is_ascii_digit())
        .take(3)
        .map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiOp {
    ListRuns,
    ListJobs,
    JobLogs,
    JobLogsAttempt,
    Cancel,
    RerunRun,
    RerunJob,
    RerunFailed,
    ListArtifacts,
    DownloadArtifact,
    ListWorkflows,
}

impl ApiOp {
    pub fn describe(self) -> &'static str {
        match self {
            Self::ListRuns => "listing workflow runs",
            Self::ListJobs => "listing the jobs of a run",
            Self::JobLogs => "downloading job logs",
            Self::JobLogsAttempt => "downloading logs of an earlier job attempt",
            Self::Cancel => "cancelling a run",
            Self::RerunRun => "rerunning a run",
            Self::RerunJob => "rerunning a job",
            Self::RerunFailed => "rerunning failed jobs",
            Self::ListArtifacts => "listing artifacts",
            Self::DownloadArtifact => "downloading artifacts",
            Self::ListWorkflows => "listing workflows",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Yes,
    /// Not available; `since` names the first release that has it, if any.
    No {
        since: Option<&'static str>,
    },
    /// Version unknown or outside the verified range: try and interpret the response.
    Unknown,
}

const FORGEJO_VERIFIED: (Version, Version) = ((15, 0, 0), (17, 0, 1));
const GITEA_VERIFIED: (Version, Version) = ((1, 25, 0), (1, 27, 3));

/// Minimum version that has the endpoint(s) for `op`, and its display label.
fn min_version(family: ServerFamily, op: ApiOp) -> Option<(Version, &'static str)> {
    use ApiOp::*;
    match family {
        ServerFamily::Forgejo => match op {
            ListRuns => Some(((15, 0, 0), "Forgejo 15")),
            ListJobs | JobLogs | JobLogsAttempt | Cancel | ListArtifacts | DownloadArtifact => {
                Some(((16, 0, 0), "Forgejo 16"))
            }
            RerunRun | RerunJob | RerunFailed => Some(((17, 0, 0), "Forgejo 17")),
            ListWorkflows => None,
        },
        ServerFamily::Gitea => match op {
            ListRuns | ListJobs | JobLogs | ListArtifacts | DownloadArtifact | ListWorkflows => {
                Some(((1, 25, 0), "Gitea 1.25"))
            }
            RerunRun | RerunJob | RerunFailed => Some(((1, 26, 0), "Gitea 1.26")),
            JobLogsAttempt => Some(((1, 27, 0), "Gitea 1.27")),
            Cancel => None,
        },
    }
}

pub fn support(server: &ServerInfo, op: ApiOp) -> Support {
    let Some(version) = server.version else {
        return Support::Unknown;
    };
    let (floor, ceiling) = match server.family {
        ServerFamily::Forgejo => FORGEJO_VERIFIED,
        ServerFamily::Gitea => GITEA_VERIFIED,
    };
    match min_version(server.family, op) {
        Some((min, _)) if version >= min => Support::Yes,
        // Endpoints are only ever added, so anything newer than the oldest
        // verified release that lacks them is missing in older releases too.
        Some((min, label)) if min > floor => Support::No { since: Some(label) },
        Some(_) => Support::Unknown,
        None if version <= ceiling => Support::No { since: None },
        None => Support::Unknown,
    }
}

/// A workflow run as reported by the API, normalized across Forgejo and Gitea.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiRun {
    /// API id (used in `/actions/runs/{id}` API paths).
    pub id: i64,
    /// The number shown in the run's web URL (what `--run-index` refers to).
    pub index: i64,
    pub status: Option<String>,
    pub html_url: Option<String>,
    pub branch: Option<String>,
    pub created_at: Option<String>,
    /// Workflow file name (e.g. `ci.yml`).
    pub workflow: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiJob {
    pub id: i64,
    pub name: Option<String>,
    pub status: Option<String>,
    pub attempt: Option<i64>,
}

/// Map an API status to the vocabulary used by the UI backend
/// (`success`, `failure`, `running`, `waiting`, `cancelled`, `skipped`, `blocked`).
pub fn normalize_status(
    family: ServerFamily,
    status: Option<&str>,
    conclusion: Option<&str>,
) -> Option<String> {
    let status = status.map(str::trim).filter(|s| !s.is_empty());
    let conclusion = conclusion.map(str::trim).filter(|s| !s.is_empty());
    match family {
        ServerFamily::Forgejo => status.map(str::to_ascii_lowercase),
        // Gitea uses GitHub-compatible status/conclusion pairs. Its converter
        // (`ToActionsStatus`) reports its own Waiting status as `queued` and
        // Blocked (awaiting approval, `needs` or concurrency) as `waiting`.
        ServerFamily::Gitea => match status.map(str::to_ascii_lowercase).as_deref() {
            Some("completed") => Some(
                conclusion
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            Some("in_progress") => Some("running".to_string()),
            Some("waiting") => Some("blocked".to_string()),
            Some("queued" | "pending" | "requested") => Some("waiting".to_string()),
            Some(other) => Some(other.to_string()),
            None => conclusion.map(str::to_ascii_lowercase),
        },
    }
}

/// Run status as the UI backend reports it in run lists (the web UI's status
/// tooltip, e.g. `Success`, `Canceled`), so output does not depend on the backend.
pub fn ui_run_status_label(status: &str) -> String {
    match status.trim().to_ascii_lowercase().as_str() {
        "cancelled" | "canceled" => "Canceled".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// The run number in a web URL like `https://host/sub/owner/repo/actions/runs/12`.
pub fn run_url_number(url: &str) -> Option<i64> {
    RUN_URL_NUMBER_RE
        .captures_iter(url)
        .last()
        .and_then(|c| c.name("n"))
        .and_then(|m| m.as_str().parse().ok())
}

pub fn parse_run(v: &Value, family: ServerFamily) -> Option<ApiRun> {
    let id = v.get("id")?.as_i64()?;
    let html_url = str_field(v, "html_url");
    let index = html_url
        .as_deref()
        .and_then(run_url_number)
        .or_else(|| v.get("index_in_repo").and_then(Value::as_i64))
        .or_else(|| v.get("run_number").and_then(Value::as_i64))
        .unwrap_or(id);
    let status = normalize_status(
        family,
        v.get("status").and_then(Value::as_str),
        v.get("conclusion").and_then(Value::as_str),
    );
    let workflow = str_field(v, "workflow_id").or_else(|| {
        // Gitea: `path` is `<file>@<ref>` (e.g. `ci.yml@refs/heads/main`).
        str_field(v, "path").map(|p| {
            let file = p.split('@').next().unwrap_or(&p);
            file.rsplit('/').next().unwrap_or(file).to_string()
        })
    });
    Some(ApiRun {
        id,
        index,
        status,
        html_url,
        branch: str_field(v, "prettyref").or_else(|| str_field(v, "head_branch")),
        created_at: str_field(v, "created").or_else(|| str_field(v, "started_at")),
        workflow,
    })
}

/// Parse a run list (`{"workflow_runs": [...]}`; both families).
pub fn parse_runs(v: &Value, family: ServerFamily) -> eyre::Result<Vec<ApiRun>> {
    let list = v
        .get("workflow_runs")
        .ok_or_else(|| eyre!("unexpected run list response (no workflow_runs field)"))?;
    let Some(items) = list.as_array() else {
        // Some versions return `null` for an empty list.
        return Ok(Vec::new());
    };
    Ok(items.iter().filter_map(|r| parse_run(r, family)).collect())
}

/// Parse a job list: a bare array (Forgejo) or `{"jobs": [...], "total_count": n}` (Gitea).
/// Jobs are returned in web UI order (ascending id).
pub fn parse_jobs(v: &Value, family: ServerFamily) -> eyre::Result<(Vec<ApiJob>, Option<i64>)> {
    let (items, total) = match v {
        Value::Array(items) => (items.as_slice(), None),
        Value::Null => (&[][..], None),
        Value::Object(map) => (
            map.get("jobs")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            map.get("total_count").and_then(Value::as_i64),
        ),
        _ => return Err(eyre!("unexpected job list response")),
    };
    let mut jobs: Vec<ApiJob> = items
        .iter()
        .filter_map(|j| {
            Some(ApiJob {
                id: j.get("id")?.as_i64()?,
                name: str_field(j, "name"),
                status: normalize_status(
                    family,
                    j.get("status").and_then(Value::as_str),
                    j.get("conclusion").and_then(Value::as_str),
                ),
                attempt: j
                    .get("attempt")
                    .and_then(Value::as_i64)
                    .or_else(|| j.get("run_attempt").and_then(Value::as_i64))
                    .filter(|a| *a > 0),
            })
        })
        .collect();
    jobs.sort_by_key(|j| j.id);
    Ok((jobs, total))
}

/// Normalize an artifact list to `{"artifacts": [{id, name, size_in_bytes, ...}]}`
/// (Forgejo returns a bare array, Gitea `{"artifacts": [...]}`).
pub fn parse_artifacts(v: &Value) -> Value {
    let items = match v {
        Value::Array(items) => items.clone(),
        Value::Object(map) => map
            .get("artifacts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    serde_json::json!({ "artifacts": items })
}

/// Append the artifacts of `batch` not already in `all` (by id, else by value).
fn merge_artifacts(all: &mut Vec<Value>, batch: Vec<Value>) {
    for artifact in batch {
        let id = artifact.get("id").and_then(Value::as_i64);
        let known = all.iter().any(|a| match id {
            Some(id) => a.get("id").and_then(Value::as_i64) == Some(id),
            None => *a == artifact,
        });
        if !known {
            all.push(artifact);
        }
    }
}

/// Pick an artifact by name, falling back to its numeric id.
pub fn find_artifact<'a>(artifacts: &'a Value, name_or_id: &str) -> Option<&'a Value> {
    let items = artifacts.get("artifacts")?.as_array()?;
    items
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name_or_id))
        .or_else(|| {
            let id: i64 = name_or_id.trim().parse().ok()?;
            items
                .iter()
                .find(|a| a.get("id").and_then(Value::as_i64) == Some(id))
        })
}

/// Workflow file names from Gitea's `{"workflows": [{"id": "ci.yml", "path": ".gitea/workflows/ci.yml"}]}`.
pub fn parse_workflows(v: &Value) -> Vec<String> {
    let mut names: Vec<String> = v
        .get("workflows")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|w| {
                    str_field(w, "path")
                        .map(|p| p.rsplit('/').next().unwrap_or(&p).to_string())
                        .or_else(|| str_field(w, "id"))
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

pub struct ApiActions {
    client: crate::api::ApiClient,
    auth_label: String,
    base_url: String,
    owner: String,
    repo: String,
    server: ServerInfo,
    run_ids: Mutex<HashMap<i64, i64>>,
}

pub struct RerunFailedOutcome {
    pub failures: Vec<String>,
    pub requested: Vec<String>,
}

impl ApiActions {
    pub async fn connect(
        target: &crate::target::ResolvedTarget,
        repo: &str,
        auth: &crate::credentials::ApiAuth,
    ) -> eyre::Result<Self> {
        let (owner, name) = repo.trim_matches('/').split_once('/').ok_or_else(|| {
            eyre!("Repo should be in the format owner/name; got '{repo}'. Pass --repo owner/name.")
        })?;
        let client = crate::api::ApiClient::from_auth(
            &target.base_url,
            auth,
            target.unix_socket.as_deref(),
        )?;
        let server = detect_server(&client, &target.base_url, &auth.source_label()).await?;
        Ok(Self {
            client,
            auth_label: auth.source_label(),
            base_url: target.base_url.clone(),
            owner: owner.to_string(),
            repo: name.to_string(),
            server,
            run_ids: Mutex::new(HashMap::new()),
        })
    }

    fn repo_path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn actions_url(&self, path: &str) -> String {
        self.client.api_v1_url(&format!(
            "/repos/{}/{}/actions{path}",
            urlencoding::encode(&self.owner),
            urlencoding::encode(&self.repo)
        ))
    }

    fn unsupported(&self, op: ApiOp, since: Option<&str>) -> eyre::Report {
        let added = since
            .map(|s| format!(" (it was added in {s})"))
            .unwrap_or_default();
        eyre!(
            "This server's REST API ({}) has no endpoint for {}{added}, so it cannot be done with an API token.\n\
             Use a UI session instead: `fj-ex auth login --host {}` (password) or \
             `fj-ex auth login --host {} --session-cookie-stdin` (SSO browser session), then rerun (or pass --backend ui).",
            self.server.label(),
            op.describe(),
            self.base_url,
            self.base_url
        )
    }

    /// Fail early when `op` is known to be unavailable on this server version.
    pub fn require(&self, op: ApiOp) -> eyre::Result<()> {
        match support(&self.server, op) {
            Support::No { since } => Err(self.unsupported(op, since)),
            Support::Yes | Support::Unknown => Ok(()),
        }
    }

    /// Send a request and map HTTP errors to actionable messages. Response
    /// bodies are never included because they may echo request data.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        op: ApiOp,
    ) -> eyre::Result<reqwest::Response> {
        let resp = request
            .send()
            .await
            .wrap_err_with(|| format!("API request failed while {}", op.describe()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        let url = resp.url().clone();
        let is_json = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("json"));
        let path = url.path();
        match status.as_u16() {
            401 => Err(eyre!(
                "HTTP 401 from {path} while {}: the {} was rejected. Check that it is current.",
                op.describe(),
                self.auth_label
            )),
            403 => Err(eyre!(
                "HTTP 403 from {path} while {}: the {} lacks permission. Reading needs the `read:repository` token scope; \
                 cancel/rerun/dispatch need `write:repository`.",
                op.describe(),
                self.auth_label
            )),
            // An unknown route answers with a plain-text 404 (or 405 for a
            // wrong method), while a missing run/job answers with JSON.
            404 | 405 if !is_json => Err(self.unsupported(op, None)),
            404 if matches!(op, ApiOp::JobLogs | ApiOp::JobLogsAttempt) => Err(eyre!(
                "HTTP 404 from {path} while {}: the job has no logs (it may not have started, \
                 e.g. it was cancelled or is still waiting for a runner).",
                op.describe()
            )),
            404 => Err(eyre!(
                "HTTP 404 from {path} while {}: not found (check the repository, run and job).",
                op.describe()
            )),
            _ => Err(eyre!(
                "HTTP {status} from {path} while {}.",
                op.describe()
            )),
        }
    }

    async fn get_value(&self, url: &str, op: ApiOp) -> eyre::Result<Value> {
        let resp = self.send(self.client.http().get(url), op).await?;
        let body = resp
            .bytes()
            .await
            .wrap_err_with(|| format!("failed to read API response while {}", op.describe()))?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).wrap_err_with(|| {
            format!(
                "unexpected (non-JSON) API response while {} (body_length={})",
                op.describe(),
                body.len()
            )
        })
    }

    async fn post_empty(&self, url: &str, op: ApiOp) -> eyre::Result<()> {
        self.send(self.client.http().post(url), op).await?;
        Ok(())
    }

    fn to_run_ref(&self, run: &ApiRun) -> RunRef {
        RunRef {
            run_index: run.index,
            url: run.html_url.clone().unwrap_or_else(|| {
                format!(
                    "{}/{}/actions/runs/{}",
                    self.base_url.trim_end_matches('/'),
                    self.repo_path(),
                    run.index
                )
            }),
            status: run.status.as_deref().map(ui_run_status_label),
            branch: run.branch.clone(),
            created_at: run.created_at.clone(),
        }
    }

    async fn runs_page(
        &self,
        page: u32,
        limit: u32,
        extra_query: &str,
    ) -> eyre::Result<Vec<ApiRun>> {
        Ok(self.runs_page_counted(page, limit, extra_query).await?.0)
    }

    /// A page of runs plus the list's `total_count`, if reported.
    async fn runs_page_counted(
        &self,
        page: u32,
        limit: u32,
        extra_query: &str,
    ) -> eyre::Result<(Vec<ApiRun>, Option<i64>)> {
        let url = self.actions_url(&format!("/runs?page={page}&limit={limit}{extra_query}"));
        let value = self.get_value(&url, ApiOp::ListRuns).await?;
        let runs = parse_runs(&value, self.server.family)?;
        self.remember(&runs);
        Ok((runs, total_count(&value)))
    }

    fn remember(&self, runs: &[ApiRun]) {
        let mut ids = self.run_ids.lock().unwrap();
        for run in runs {
            ids.insert(run.index, run.id);
        }
    }

    pub async fn list_runs(
        &self,
        workflow: Option<&str>,
        page: u32,
        limit: u32,
    ) -> eyre::Result<Vec<RunRef>> {
        self.require(ApiOp::ListRuns)?;
        let workflow = workflow.map(str::trim).filter(|w| !w.is_empty());
        let mut runs = match workflow {
            None => self.runs_page(page, limit, "").await?,
            Some(wf) if self.server.family == ServerFamily::Forgejo => {
                let query = format!("&workflow_id={}", urlencoding::encode(wf));
                let mut runs = self.runs_page(page, limit, &query).await?;
                runs.retain(|r| workflow_matches(r, wf));
                runs
            }
            Some(wf) if self.server.version.is_some_and(|v| v >= (1, 27, 0)) => {
                let url = self.actions_url(&format!(
                    "/workflows/{}/runs?page={page}&limit={limit}",
                    urlencoding::encode(wf)
                ));
                let value = self.get_value(&url, ApiOp::ListRuns).await?;
                let runs = parse_runs(&value, self.server.family)?;
                self.remember(&runs);
                runs
            }
            Some(wf) => {
                // Gitea < 1.27 cannot filter by workflow server-side.
                let skip = (page.saturating_sub(1) as usize) * limit as usize;
                let wanted = skip + limit as usize;
                let mut matched = Vec::new();
                let mut scan = PageScan::default();
                for server_page in 1..=MAX_SCAN_PAGES {
                    let (batch, total) = self
                        .runs_page_counted(server_page, SCAN_PAGE_SIZE, "")
                        .await?;
                    let done = !scan.more(batch.len(), total, SCAN_PAGE_SIZE);
                    matched.extend(batch.into_iter().filter(|r| workflow_matches(r, wf)));
                    if done || matched.len() >= wanted {
                        break;
                    }
                }
                matched
                    .into_iter()
                    .skip(skip)
                    .take(limit as usize)
                    .collect()
            }
        };
        runs.sort_by_key(|r| std::cmp::Reverse(r.index));
        Ok(runs.iter().map(|r| self.to_run_ref(r)).collect())
    }

    pub async fn latest_run_index(&self, workflow: Option<&str>) -> eyre::Result<Option<i64>> {
        Ok(self
            .list_runs(workflow, 1, 1)
            .await?
            .first()
            .map(|r| r.run_index))
    }

    async fn get_run_by_id(&self, id: i64) -> eyre::Result<Option<ApiRun>> {
        let url = self.actions_url(&format!("/runs/{id}"));
        let resp = self
            .client
            .http()
            .get(&url)
            .send()
            .await
            .wrap_err("API request failed while looking up a run")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let is_json = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("json"));
            if is_json {
                return Ok(None);
            }
            return Err(self.unsupported(ApiOp::ListRuns, None));
        }
        let resp = self.send_checked(resp, ApiOp::ListRuns)?;
        let value: Value = resp
            .json()
            .await
            .wrap_err("unexpected (non-JSON) API response while looking up a run")?;
        Ok(parse_run(&value, self.server.family))
    }

    fn send_checked(&self, resp: reqwest::Response, op: ApiOp) -> eyre::Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let path = resp.url().path().to_string();
        Err(match status.as_u16() {
            401 => eyre!(
                "HTTP 401 from {path} while {}: the {} was rejected.",
                op.describe(),
                self.auth_label
            ),
            403 => eyre!(
                "HTTP 403 from {path} while {}: the {} lacks permission (needs the `read:repository` scope).",
                op.describe(),
                self.auth_label
            ),
            _ => eyre!("HTTP {status} from {path} while {}.", op.describe()),
        })
    }

    /// Resolve the number from a run's web URL to the API run.
    pub async fn find_run(&self, index: i64) -> eyre::Result<ApiRun> {
        self.require(ApiOp::ListRuns)?;
        let cached = self.run_ids.lock().unwrap().get(&index).copied();
        if let Some(id) = cached {
            if let Some(run) = self.get_run_by_id(id).await? {
                if run.index == index {
                    return Ok(run);
                }
            }
        }

        match self.server.family {
            ServerFamily::Forgejo => {
                let query = format!("&run_number={index}");
                let runs = self.runs_page(1, SCAN_PAGE_SIZE, &query).await?;
                if let Some(run) = runs.into_iter().find(|r| r.index == index) {
                    return Ok(run);
                }
            }
            ServerFamily::Gitea => {
                // Gitea >= 1.26 puts the run id in web URLs.
                if let Some(run) = self.get_run_by_id(index).await? {
                    if run.index == index {
                        self.remember(std::slice::from_ref(&run));
                        return Ok(run);
                    }
                }
            }
        }

        let mut scan = PageScan::default();
        for page in 1..=MAX_SCAN_PAGES {
            let (runs, total) = self.runs_page_counted(page, SCAN_PAGE_SIZE, "").await?;
            if let Some(run) = runs.iter().find(|r| r.index == index) {
                return Ok(run.clone());
            }
            let exhausted = !scan.more(runs.len(), total, SCAN_PAGE_SIZE);
            // Lists are newest first; once every run is older, stop.
            let all_older = !runs.is_empty() && runs.iter().all(|r| r.index < index);
            if exhausted || all_older {
                break;
            }
        }

        Err(eyre!(
            "No action run #{index} found in {} (looked it up through the REST API).",
            self.repo_path()
        ))
    }

    pub async fn run_status(&self, index: i64) -> eyre::Result<Option<String>> {
        Ok(self
            .find_run(index)
            .await?
            .status
            .as_deref()
            .map(ui_run_status_label))
    }

    pub fn run_url(&self, index: i64) -> String {
        format!(
            "{}/{}/actions/runs/{index}",
            self.base_url.trim_end_matches('/'),
            self.repo_path()
        )
    }

    async fn jobs_for_run_id(&self, run_id: i64) -> eyre::Result<Vec<ApiJob>> {
        self.require(ApiOp::ListJobs)?;
        match self.server.family {
            ServerFamily::Forgejo => {
                let url = self.actions_url(&format!("/runs/{run_id}/jobs"));
                let value = self.get_value(&url, ApiOp::ListJobs).await?;
                Ok(parse_jobs(&value, self.server.family)?.0)
            }
            ServerFamily::Gitea => {
                self.paged_jobs(&format!("/runs/{run_id}/jobs"), ApiOp::ListJobs)
                    .await
            }
        }
    }

    async fn paged_jobs(&self, path: &str, op: ApiOp) -> eyre::Result<Vec<ApiJob>> {
        let mut all = Vec::new();
        let mut scan = PageScan::default();
        for page in 1..=MAX_SCAN_PAGES {
            let url = self.actions_url(&format!("{path}?page={page}&limit={SCAN_PAGE_SIZE}"));
            let value = self.get_value(&url, op).await?;
            let (jobs, total) = parse_jobs(&value, self.server.family)?;
            let more = scan.more(jobs.len(), total, SCAN_PAGE_SIZE);
            all.extend(jobs);
            if !more {
                break;
            }
        }
        all.sort_by_key(|j| j.id);
        all.dedup_by_key(|j| j.id);
        Ok(all)
    }

    pub async fn run_jobs(&self, index: i64) -> eyre::Result<Vec<JobInfo>> {
        let run = self.find_run(index).await?;
        let jobs = self.jobs_for_run_id(run.id).await?;
        Ok(jobs
            .into_iter()
            .enumerate()
            .map(|(pos, job)| JobInfo {
                run_index: index,
                job_index: pos as i64,
                id: Some(job.id),
                name: job.name,
                status: job.status,
                can_rerun: None,
                duration: None,
            })
            .collect())
    }

    async fn job_at(&self, index: i64, job_index: i64) -> eyre::Result<(ApiRun, ApiJob)> {
        let run = self.find_run(index).await?;
        let jobs = self.jobs_for_run_id(run.id).await?;
        let count = jobs.len();
        let job = usize::try_from(job_index)
            .ok()
            .and_then(|i| jobs.into_iter().nth(i))
            .ok_or_else(|| {
                eyre!("Run #{index} has no job index {job_index} (it has {count} job(s)).")
            })?;
        Ok((run, job))
    }

    pub async fn job_attempt(&self, index: i64, job_index: i64) -> eyre::Result<i64> {
        // Only needed to download logs; report the logs capability first.
        self.require(ApiOp::JobLogs)?;
        let (_, job) = self.job_at(index, job_index).await?;
        Ok(job.attempt.unwrap_or(1))
    }

    pub async fn open_job_logs(
        &self,
        index: i64,
        job_index: i64,
        attempt: i64,
    ) -> eyre::Result<reqwest::Response> {
        self.require(ApiOp::JobLogs)?;
        let (run, job) = self.job_at(index, job_index).await?;
        let url = match self.server.family {
            ServerFamily::Forgejo => {
                self.actions_url(&format!("/jobs/{}/logs?attempt={attempt}", job.id))
            }
            ServerFamily::Gitea if job.attempt.is_none_or(|current| current == attempt) => {
                self.actions_url(&format!("/jobs/{}/logs", job.id))
            }
            ServerFamily::Gitea => {
                // Gitea's job logs endpoint only serves the job row it is given;
                // earlier attempts have their own job rows (Gitea >= 1.27).
                self.require(ApiOp::JobLogsAttempt)?;
                let jobs = self
                    .paged_jobs(
                        &format!("/runs/{}/attempts/{attempt}/jobs", run.id),
                        ApiOp::JobLogsAttempt,
                    )
                    .await?;
                let job = usize::try_from(job_index)
                    .ok()
                    .and_then(|i| jobs.into_iter().nth(i))
                    .ok_or_else(|| {
                        eyre!("Run #{index} attempt {attempt} has no job index {job_index}.")
                    })?;
                self.actions_url(&format!("/jobs/{}/logs", job.id))
            }
        };
        self.send(self.client.http().get(&url), ApiOp::JobLogs)
            .await
    }

    pub async fn run_artifacts(&self, index: i64) -> eyre::Result<Value> {
        self.require(ApiOp::ListArtifacts)?;
        let run = self.find_run(index).await?;
        // The list is paged (Gitea's default page size is 30). A server that
        // ignores the paging parameters returns the same items again, which
        // ends the scan.
        let mut all: Vec<Value> = Vec::new();
        let mut scan = PageScan::default();
        for page in 1..=MAX_SCAN_PAGES {
            let url = self.actions_url(&format!(
                "/runs/{}/artifacts?page={page}&limit={SCAN_PAGE_SIZE}",
                run.id
            ));
            let value = self.get_value(&url, ApiOp::ListArtifacts).await?;
            let batch = parse_artifacts(&value)["artifacts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let got = batch.len();
            let before = all.len();
            merge_artifacts(&mut all, batch);
            if all.len() == before || !scan.more(got, total_count(&value), SCAN_PAGE_SIZE) {
                break;
            }
        }
        Ok(serde_json::json!({ "artifacts": all }))
    }

    pub async fn open_artifact(
        &self,
        index: i64,
        name_or_id: &str,
    ) -> eyre::Result<reqwest::Response> {
        self.require(ApiOp::DownloadArtifact)?;
        let artifacts = self.run_artifacts(index).await?;
        let artifact = find_artifact(&artifacts, name_or_id).ok_or_else(|| {
            let hint = self
                .artifacts_hint()
                .map(|h| format!("\n{h}"))
                .unwrap_or_default();
            eyre!("Run #{index} has no artifact named (or with id) '{name_or_id}'.{hint}")
        })?;
        let id = artifact
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| eyre!("artifact '{name_or_id}' has no id in the API response"))?;
        let url = self.actions_url(&format!("/artifacts/{id}/zip"));
        // Gitea answers with a redirect to a signed blob URL; reqwest follows it
        // and drops the Authorization header if the redirect leaves the host.
        self.send(self.client.http().get(&url), ApiOp::DownloadArtifact)
            .await
    }

    /// What the REST artifact list leaves out on this server, if anything.
    pub fn artifacts_hint(&self) -> Option<String> {
        (self.server.family == ServerFamily::Gitea).then(|| {
            format!(
                "note: Gitea's REST API only lists artifacts uploaded with the v4 protocol \
                 (actions/upload-artifact@v4); v3 uploads need a UI session: \
                 `fj-ex auth login --host {}` (or --session-cookie-stdin), then rerun with --backend ui.",
                self.base_url
            )
        })
    }

    pub async fn list_workflows(&self, page: u32, limit: u32) -> eyre::Result<Vec<String>> {
        self.require(ApiOp::ListWorkflows)?;
        let url = self.actions_url("/workflows");
        let value = self.get_value(&url, ApiOp::ListWorkflows).await?;
        let skip = (page.saturating_sub(1) as usize) * limit as usize;
        Ok(parse_workflows(&value)
            .into_iter()
            .skip(skip)
            .take(limit as usize)
            .collect())
    }

    pub async fn cancel_url(&self, index: i64) -> eyre::Result<String> {
        self.require(ApiOp::Cancel)?;
        let run = self.find_run(index).await?;
        Ok(self.actions_url(&format!("/runs/{}/cancel", run.id)))
    }

    pub async fn cancel(&self, index: i64) -> eyre::Result<String> {
        let url = self.cancel_url(index).await?;
        self.post_empty(&url, ApiOp::Cancel).await?;
        Ok(url)
    }

    pub async fn rerun_url(&self, index: i64, job_index: Option<i64>) -> eyre::Result<String> {
        match job_index {
            None => {
                self.require(ApiOp::RerunRun)?;
                let run = self.find_run(index).await?;
                Ok(self.actions_url(&format!("/runs/{}/rerun", run.id)))
            }
            Some(job_index) => {
                self.require(ApiOp::RerunJob)?;
                let (run, job) = self.job_at(index, job_index).await?;
                Ok(self.job_rerun_url(&run, &job))
            }
        }
    }

    fn job_rerun_url(&self, run: &ApiRun, job: &ApiJob) -> String {
        match self.server.family {
            ServerFamily::Forgejo => self.actions_url(&format!("/jobs/{}/rerun", job.id)),
            ServerFamily::Gitea => {
                self.actions_url(&format!("/runs/{}/jobs/{}/rerun", run.id, job.id))
            }
        }
    }

    pub async fn rerun(&self, index: i64, job_index: Option<i64>) -> eyre::Result<String> {
        let url = self.rerun_url(index, job_index).await?;
        let op = if job_index.is_some() {
            ApiOp::RerunJob
        } else {
            ApiOp::RerunRun
        };
        self.post_empty(&url, op).await?;
        Ok(url)
    }

    /// The requests a failed-jobs rerun would make (for `--dry-run`).
    pub async fn rerun_failed_plan(&self, index: i64, failed: &[i64]) -> eyre::Result<Vec<String>> {
        self.require(ApiOp::RerunFailed)?;
        let run = self.find_run(index).await?;
        match self.server.family {
            ServerFamily::Gitea => {
                Ok(vec![self.actions_url(&format!(
                    "/runs/{}/rerun-failed-jobs",
                    run.id
                ))])
            }
            ServerFamily::Forgejo => {
                let jobs = self.jobs_for_run_id(run.id).await?;
                Ok(failed
                    .iter()
                    .filter_map(|i| usize::try_from(*i).ok().and_then(|i| jobs.get(i)))
                    .map(|job| self.job_rerun_url(&run, job))
                    .collect())
            }
        }
    }

    pub async fn rerun_failed(
        &self,
        index: i64,
        failed: &[i64],
    ) -> eyre::Result<RerunFailedOutcome> {
        let urls = self.rerun_failed_plan(index, failed).await?;
        let mut outcome = RerunFailedOutcome {
            failures: Vec::new(),
            requested: Vec::new(),
        };
        for url in urls {
            match self.post_empty(&url, ApiOp::RerunFailed).await {
                Ok(()) => outcome.requested.push(url),
                Err(err) => outcome.failures.push(format!("POST {url} -> {err}")),
            }
        }
        Ok(outcome)
    }
}

fn workflow_matches(run: &ApiRun, workflow: &str) -> bool {
    run.workflow
        .as_deref()
        .is_some_and(|w| w == workflow || w.rsplit('/').next() == Some(workflow))
}

async fn detect_server(
    client: &crate::api::ApiClient,
    base_url: &str,
    auth_label: &str,
) -> eyre::Result<ServerInfo> {
    let url = client.api_v1_url("/version");
    let resp = client
        .http()
        .get(&url)
        .send()
        .await
        .wrap_err_with(|| format!("failed to reach the REST API of '{base_url}'"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(eyre!(
            "The {auth_label} was rejected by '{base_url}' (HTTP 401). Check that it is current."
        ));
    }
    if !status.is_success() {
        return Err(eyre!(
            "GET {url} returned HTTP {status}; is '{base_url}' a Forgejo/Gitea instance?"
        ));
    }
    let value: Value = resp
        .json()
        .await
        .wrap_err_with(|| format!("unexpected response from {url}"))?;
    let raw = value
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let forgejo_probe = if raw.contains("+gitea") {
        true
    } else {
        let forgejo_url = url.replace("/api/v1/version", "/api/forgejo/v1/version");
        client
            .http()
            .get(&forgejo_url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    };
    Ok(ServerInfo::from_version(&raw, forgejo_probe))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "forgejo-15-runs" => include_str!("fixtures/api/forgejo-15-runs.json"),
            "forgejo-17-runs" => include_str!("fixtures/api/forgejo-17-runs.json"),
            "forgejo-17-jobs" => include_str!("fixtures/api/forgejo-17-jobs.json"),
            "gitea-1.25-runs" => include_str!("fixtures/api/gitea-1.25-runs.json"),
            "gitea-1.27-runs" => include_str!("fixtures/api/gitea-1.27-runs.json"),
            "gitea-1.27-jobs" => include_str!("fixtures/api/gitea-1.27-jobs.json"),
            other => panic!("unknown fixture {other}"),
        };
        serde_json::from_str(raw).unwrap()
    }

    fn server(raw: &str, probe: bool) -> ServerInfo {
        ServerInfo::from_version(raw, probe)
    }

    #[test]
    fn detects_server_family_and_version() {
        let fj = server("17.0.1+gitea-1.22.0", false);
        assert_eq!(fj.family, ServerFamily::Forgejo);
        assert_eq!(fj.version, Some((17, 0, 1)));
        assert_eq!(fj.label(), "Forgejo 17.0.1");

        let gt = server("1.27.3", false);
        assert_eq!(gt.family, ServerFamily::Gitea);
        assert_eq!(gt.version, Some((1, 27, 3)));

        let dev = server("1.26.0+dev-123-gabcdef", false);
        assert_eq!(dev.family, ServerFamily::Gitea);
        assert_eq!(dev.version, Some((1, 26, 0)));

        assert_eq!(server("16.0.0-dev", true).family, ServerFamily::Forgejo);
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn capability_matrix_matches_verified_servers() {
        use ApiOp::*;
        let yes = |raw: &str, op| support(&server(raw, false), op) == Support::Yes;
        let no = |raw: &str, op| matches!(support(&server(raw, false), op), Support::No { .. });

        // Forgejo 15: runs only.
        assert!(yes("15.0.11+gitea-1.22.0", ListRuns));
        for op in [
            ListJobs,
            JobLogs,
            Cancel,
            RerunRun,
            RerunFailed,
            ListArtifacts,
        ] {
            assert!(no("15.0.11+gitea-1.22.0", op), "{op:?}");
        }
        // Forgejo 16: jobs, logs, cancel, artifacts; no rerun.
        for op in [
            ListJobs,
            JobLogs,
            JobLogsAttempt,
            Cancel,
            ListArtifacts,
            DownloadArtifact,
        ] {
            assert!(yes("16.0.7+gitea-1.22.0", op), "{op:?}");
        }
        assert!(no("16.0.7+gitea-1.22.0", RerunRun));
        assert!(no("16.0.7+gitea-1.22.0", RerunFailed));
        // Forgejo 17: reruns too, still no workflow listing.
        for op in [RerunRun, RerunJob, RerunFailed, Cancel] {
            assert!(yes("17.0.1+gitea-1.22.0", op), "{op:?}");
        }
        assert!(no("17.0.1+gitea-1.22.0", ListWorkflows));

        // Gitea 1.25: read-only; 1.26 adds reruns; cancel never (as of 1.27).
        for op in [ListRuns, ListJobs, JobLogs, ListArtifacts, ListWorkflows] {
            assert!(yes("1.25.5", op), "{op:?}");
        }
        assert!(no("1.25.5", RerunFailed));
        assert!(no("1.25.5", JobLogsAttempt));
        assert!(yes("1.26.4", RerunFailed));
        assert!(no("1.26.4", JobLogsAttempt));
        assert!(yes("1.27.3", JobLogsAttempt));
        assert!(no("1.27.3", Cancel));

        // Outside the verified range the endpoint is probed.
        assert_eq!(support(&server("1.30.0", false), Cancel), Support::Unknown);
        assert_eq!(
            support(&server("1.24.0", false), ListRuns),
            Support::Unknown
        );
        assert_eq!(support(&server("", false), ListRuns), Support::Unknown);
        // A Forgejo older than the oldest verified release lacks newer endpoints.
        assert!(no("14.0.2+gitea-1.22.0", RerunRun));
    }

    #[test]
    fn forgejo_runs_use_index_in_repo_from_html_url() {
        let runs = parse_runs(&fixture("forgejo-17-runs"), ServerFamily::Forgejo).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].id, runs[0].index), (3, 2));
        assert_eq!(runs[0].status.as_deref(), Some("failure"));
        assert_eq!(runs[0].workflow.as_deref(), Some("ci.yml"));
        assert_eq!(runs[0].branch.as_deref(), Some("main"));
        assert!(runs[0].created_at.is_some());

        let runs = parse_runs(&fixture("forgejo-15-runs"), ServerFamily::Forgejo).unwrap();
        assert_eq!((runs[0].id, runs[0].index), (3, 2));
    }

    #[test]
    fn gitea_run_number_in_url_differs_by_version() {
        // Gitea 1.25 web URLs carry the run number ...
        let runs = parse_runs(&fixture("gitea-1.25-runs"), ServerFamily::Gitea).unwrap();
        assert_eq!((runs[0].id, runs[0].index), (3, 2));
        assert_eq!(runs[0].status.as_deref(), Some("failure"));
        assert_eq!(runs[0].workflow.as_deref(), Some("ci.yml"));
        assert_eq!(runs[0].branch.as_deref(), Some("main"));
        // ... while Gitea >= 1.26 web URLs carry the run id.
        let runs = parse_runs(&fixture("gitea-1.27-runs"), ServerFamily::Gitea).unwrap();
        assert_eq!((runs[0].id, runs[0].index), (3, 3));
    }

    #[test]
    fn job_lists_are_normalized_and_ordered() {
        let (jobs, total) = parse_jobs(&fixture("forgejo-17-jobs"), ServerFamily::Forgejo).unwrap();
        assert_eq!(total, None);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].name.as_deref(), Some("ok"));
        assert_eq!(jobs[1].status.as_deref(), Some("failure"));
        assert_eq!(jobs[1].attempt, Some(2));

        let (jobs, total) = parse_jobs(&fixture("gitea-1.27-jobs"), ServerFamily::Gitea).unwrap();
        assert_eq!(total, Some(2));
        assert_eq!(jobs[0].status.as_deref(), Some("success"));
        assert_eq!(jobs[1].status.as_deref(), Some("failure"));
        assert_eq!(jobs[1].attempt, Some(2));

        let unordered = serde_json::json!([{"id": 9, "name": "b"}, {"id": 4, "name": "a"}]);
        let (jobs, _) = parse_jobs(&unordered, ServerFamily::Forgejo).unwrap();
        assert_eq!(jobs[0].id, 4);
        assert!(parse_jobs(&Value::Null, ServerFamily::Forgejo)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn gitea_statuses_map_to_ui_vocabulary() {
        let g = |s: Option<&str>, c: Option<&str>| normalize_status(ServerFamily::Gitea, s, c);
        assert_eq!(
            g(Some("completed"), Some("success")).as_deref(),
            Some("success")
        );
        assert_eq!(
            g(Some("completed"), Some("cancelled")).as_deref(),
            Some("cancelled")
        );
        assert_eq!(g(Some("in_progress"), None).as_deref(), Some("running"));
        assert_eq!(g(Some("queued"), None).as_deref(), Some("waiting"));
        // Gitea reports its Blocked status as GitHub's `waiting`.
        assert_eq!(g(Some("waiting"), None).as_deref(), Some("blocked"));
        assert_eq!(ui_run_status_label("blocked"), "Blocked");
        assert_eq!(
            normalize_status(ServerFamily::Forgejo, Some("Running"), None).as_deref(),
            Some("running")
        );
    }

    #[test]
    fn run_list_statuses_use_the_ui_labels() {
        assert_eq!(ui_run_status_label("failure"), "Failure");
        assert_eq!(ui_run_status_label("cancelled"), "Canceled");
        assert_eq!(ui_run_status_label("running"), "Running");
        assert_eq!(ui_run_status_label("Success"), "Success");
    }

    #[test]
    fn run_url_numbers_support_sub_paths() {
        assert_eq!(
            run_url_number("https://h/gitea/o/r/actions/runs/12"),
            Some(12)
        );
        assert_eq!(
            run_url_number("http://h/o/r/actions/runs/2/jobs/0/attempt/1"),
            Some(2)
        );
        assert_eq!(run_url_number("http://h/o/r/actions"), None);
    }

    #[test]
    fn artifact_lists_are_normalized_across_families() {
        // Forgejo: bare array (schema ActionArtifact).
        let forgejo = serde_json::json!([
            {"id": 7, "name": "my-artifact", "size_in_bytes": 42, "run_id": 3, "expired": false}
        ]);
        // Gitea: {"artifacts": [...], "total_count": n} (schema ActionArtifactsResponse).
        let gitea = serde_json::json!({
            "artifacts": [{"id": 8, "name": "my-artifact", "size_in_bytes": 42, "expired": false}],
            "total_count": 1
        });
        for (value, id) in [(forgejo, 7), (gitea, 8)] {
            let normalized = parse_artifacts(&value);
            assert_eq!(normalized["artifacts"].as_array().unwrap().len(), 1);
            let found = find_artifact(&normalized, "my-artifact").unwrap();
            assert_eq!(found["id"], id);
            assert!(find_artifact(&normalized, &id.to_string()).is_some());
            assert!(find_artifact(&normalized, "missing").is_none());
        }
        assert_eq!(
            parse_artifacts(&Value::Null)["artifacts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn page_scans_survive_servers_that_cap_the_page_size() {
        // MAX_RESPONSE_ITEMS=30 with 70 items: total_count keeps the scan going.
        let mut scan = PageScan::default();
        assert!(scan.more(30, Some(70), 50));
        assert!(scan.more(30, Some(70), 50));
        assert!(!scan.more(10, Some(70), 50));

        // Without total_count, a short first page is confirmed with one more
        // request; later pages end the scan when shorter than the first.
        let mut scan = PageScan::default();
        assert!(scan.more(30, None, 50));
        assert!(scan.more(30, None, 50));
        assert!(!scan.more(5, None, 50));
        let mut scan = PageScan::default();
        assert!(scan.more(3, None, 50));
        assert!(!scan.more(0, None, 50));

        // A full page without a total keeps going; an empty page always ends.
        let mut scan = PageScan::default();
        assert!(scan.more(50, None, 50));
        assert!(!scan.more(49, None, 50));
        assert!(!PageScan::default().more(0, Some(10), 50));
        // The total is reached exactly.
        let mut scan = PageScan::default();
        assert!(!scan.more(2, Some(2), 50));
    }

    #[test]
    fn artifact_pages_are_merged_by_id() {
        let mut all = Vec::new();
        merge_artifacts(
            &mut all,
            vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})],
        );
        // A server that ignores `page` repeats the first page: nothing is added.
        merge_artifacts(
            &mut all,
            vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})],
        );
        assert_eq!(all.len(), 2);
        merge_artifacts(&mut all, vec![serde_json::json!({"id": 3})]);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn gitea_workflow_list_yields_file_names() {
        let value = serde_json::json!({
            "total_count": 2,
            "workflows": [
                {"id": "ci.yml", "name": "ci", "path": ".gitea/workflows/ci.yml", "state": "active"},
                {"id": "release.yaml", "name": "release", "path": ".gitea/workflows/release.yaml"}
            ]
        });
        assert_eq!(parse_workflows(&value), vec!["ci.yml", "release.yaml"]);
    }

    #[test]
    fn workflow_filter_matches_file_names() {
        let run = ApiRun {
            id: 1,
            index: 1,
            status: None,
            html_url: None,
            branch: None,
            created_at: None,
            workflow: Some("ci.yml".into()),
        };
        assert!(workflow_matches(&run, "ci.yml"));
        assert!(!workflow_matches(&run, "release.yml"));
    }
}
