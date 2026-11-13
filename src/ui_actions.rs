use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use eyre::{eyre, Context};
use regex::Regex;
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use url::Url;

use crate::{html, session::UiSession};

/// Maximum bytes before a run-href match to search for its status tooltip.
const STATUS_LOOKBACK_BYTES: usize = 800;
/// Maximum bytes after a run-href match to search for branch/created_at metadata.
const RUN_BLOCK_LOOKAHEAD_BYTES: usize = 8_000;

static WORKFLOW_HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"href="\?workflow=([^"&]+)"#).expect("WORKFLOW_HREF_RE regex must be valid")
});
static RUN_HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"href="/(?P<repo>[^"]+)/actions/runs/(?P<idx>\d+)""#)
        .expect("RUN_HREF_RE regex must be valid")
});
static STATUS_TOOLTIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"data-tooltip-content="(?P<status>Success|Failure|Running|Waiting|Cancelling|Canceled|Cancelled|Skipped|Blocked)""#,
    )
    .expect("STATUS_TOOLTIP_RE regex must be valid")
});
static RUN_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"class="ui label run-list-ref[^"]*"[^>]*>(?P<ref>[^<]+)</a>"#)
        .expect("RUN_REF_RE regex must be valid")
});
static CREATED_AT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<(?:relative-time|time)[^>]*datetime=['"](?P<dt>[^'"]+)['"]"#)
        .expect("CREATED_AT_RE regex must be valid")
});
static ATTEMPT_LOG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"/attempt/(?P<n>\d+)/logs"#).expect("ATTEMPT_LOG_RE regex must be valid")
});

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn repo_href_path(base_url: &str, repo_path: &str) -> String {
    let repo_path = repo_path.trim_matches('/');
    let Some(base_path) = Url::parse(base_url)
        .ok()
        .map(|url| url.path().trim_matches('/').to_string())
        .filter(|path| !path.is_empty())
    else {
        return repo_path.to_string();
    };

    format!("{base_path}/{repo_path}")
}

fn repo_href_matches(repo_href: &str, repo_path: &str, base_url: &str) -> bool {
    let repo_href = repo_href.trim_matches('/');
    repo_href == repo_path || repo_href == repo_href_path(base_url, repo_path)
}

fn next_run_href_start(html: &str, start: usize, repo_path: &str, base_url: &str) -> Option<usize> {
    RUN_HREF_RE
        .captures_iter(&html[start..])
        .filter_map(|caps| {
            let m = caps.get(0)?;
            let repo = caps.name("repo")?.as_str();
            repo_href_matches(repo, repo_path, base_url).then_some(start + m.start())
        })
        .next()
}

fn absolute_url(base_url: &str, raw_url: &str) -> String {
    if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
        return raw_url.to_string();
    }

    let base = format!("{}/", base_url.trim_end_matches('/'));
    Url::parse(&base)
        .and_then(|url| url.join(raw_url))
        .map(|url| url.to_string())
        .unwrap_or_else(|_| {
            if raw_url.starts_with('/') {
                format!("{}{}", base_url.trim_end_matches('/'), raw_url)
            } else {
                format!("{}/{}", base_url.trim_end_matches('/'), raw_url)
            }
        })
}

#[derive(Clone, Debug)]
pub struct RunRef {
    pub run_index: i64,
    pub url: String,
    pub status: Option<String>,
    pub branch: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunViewMeta {
    pub url: String,
    pub run_index: i64,
    pub run_id: Option<i64>,
    pub job_index: Option<i64>,
    pub attempt_number: Option<i64>,
    pub actions_url: Option<String>,
    /// Gitea >= 1.26 addresses runs and jobs by database id in Actions web
    /// URLs (`/runs/{run_id}/jobs/{job_id}`); Forgejo and older Gitea use
    /// the run index and the 0-based job index.
    pub id_based_urls: bool,
}

#[derive(Clone, Debug)]
pub struct RunViewData {
    pub meta: RunViewMeta,
    pub view: Value,
    pub artifacts: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct JobInfo {
    pub run_index: i64,
    pub job_index: i64,
    pub id: Option<i64>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub can_rerun: Option<bool>,
    pub duration: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct JobViewMeta {
    pub url: String,
    pub run_index: i64,
    pub run_id: Option<i64>,
    pub job_index: i64,
    pub attempt_number: i64,
    pub actions_url: Option<String>,
}

pub async fn list_workflows(
    session: &UiSession,
    repo: &str,
    page: u32,
    limit: u32,
) -> eyre::Result<Vec<String>> {
    if limit == 0 {
        return Err(eyre!("limit must be >= 1"));
    }

    let repo_path = repo.trim_matches('/');
    let url = format!(
        "{}/{repo_path}/actions?page={page}&limit={limit}&list_inner=true",
        session.base_url()
    );

    let resp = session.get_response(&url, true).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre!(
            "Failed to load workflows from '{}'. HTTP {}.",
            url,
            resp.status()
        ));
    }
    let html_s = resp.text().await.wrap_err("failed to read response html")?;

    let mut names = BTreeSet::new();
    for caps in WORKFLOW_HREF_RE.captures_iter(&html_s) {
        let raw = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        if raw.is_empty() {
            continue;
        }
        names.insert(html::html_decode(raw));
    }

    Ok(names.into_iter().collect())
}

pub async fn list_runs(
    session: &UiSession,
    repo: &str,
    workflow: Option<&str>,
    page: u32,
    limit: u32,
) -> eyre::Result<Vec<RunRef>> {
    if limit == 0 {
        return Err(eyre!("limit must be >= 1"));
    }

    let repo_path = repo.trim_matches('/');
    let workflow_param = workflow
        .filter(|s| !s.trim().is_empty())
        .map(|w| format!("&workflow={}", urlencoding::encode(w)))
        .unwrap_or_default();

    let url = format!(
        "{}/{repo_path}/actions?page={page}&limit={limit}&list_inner=true{workflow_param}",
        session.base_url()
    );

    let resp = session.get_response(&url, true).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre!(
            "Failed to list runs from '{}'. HTTP {}.",
            url,
            resp.status()
        ));
    }
    let html_s = resp.text().await.wrap_err("failed to read response html")?;

    let mut seen = HashSet::new();
    let mut runs = Vec::new();
    for caps in RUN_HREF_RE.captures_iter(&html_s) {
        let m = caps
            .get(0)
            .ok_or_else(|| eyre!("run regex capture missing match"))?;
        let repo_m = caps.name("repo").map(|m| m.as_str()).unwrap_or_default();
        if !repo_href_matches(repo_m, repo_path, session.base_url()) {
            continue;
        }
        let idx_s = caps.name("idx").map(|m| m.as_str()).unwrap_or_default();
        if idx_s.is_empty() {
            continue;
        }
        if !seen.insert(idx_s.to_string()) {
            continue;
        }
        let run_index: i64 = idx_s.parse().unwrap_or(0);
        if run_index <= 0 {
            continue;
        }

        let before_start =
            floor_char_boundary(&html_s, m.start().saturating_sub(STATUS_LOOKBACK_BYTES));
        let before = &html_s[before_start..m.end()];
        let status = STATUS_TOOLTIP_RE
            .captures_iter(before)
            .last()
            .and_then(|c| c.name("status").map(|m| m.as_str().to_string()));

        let max_after_end = floor_char_boundary(
            &html_s,
            (m.end() + RUN_BLOCK_LOOKAHEAD_BYTES).min(html_s.len()),
        );
        let after_end = next_run_href_start(&html_s, m.end(), repo_path, session.base_url())
            .map(|idx| idx.min(max_after_end))
            .unwrap_or(max_after_end);
        let after = &html_s[m.end()..after_end];

        let branch = RUN_REF_RE
            .captures(after)
            .and_then(|c| c.name("ref").map(|m| html::html_decode(m.as_str())));
        let created_at = CREATED_AT_RE
            .captures(after)
            .and_then(|c| c.name("dt").map(|m| m.as_str().to_string()));

        let run_url = format!(
            "{}/{repo_path}/actions/runs/{run_index}",
            session.base_url()
        );
        runs.push(RunRef {
            run_index,
            url: run_url,
            status,
            branch,
            created_at,
        });
    }

    runs.sort_by_key(|r| std::cmp::Reverse(r.run_index));
    Ok(runs)
}

pub async fn get_run_view_data(
    session: &UiSession,
    repo: &str,
    run_index: i64,
) -> eyre::Result<RunViewData> {
    let repo_path = repo.trim_matches('/');
    let url = format!(
        "{}/{repo_path}/actions/runs/{run_index}",
        session.base_url()
    );

    let resp = session.get_response(&url, true).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre!(
            "Failed to load run view from '{}'. HTTP {}.",
            url,
            resp.status()
        ));
    }
    let html_s = resp.text().await.wrap_err("failed to read response html")?;

    let initial_job_json = html::get_html_attribute_value(&html_s, "data-initial-post-response")
        .filter(|s| !s.trim().is_empty());
    let initial_artifacts_json =
        html::get_html_attribute_value(&html_s, "data-initial-artifacts-response");

    let embedded_view = initial_job_json
        .map(|job_json| {
            serde_json::from_str(&job_json)
                .wrap_err("failed to parse data-initial-post-response json")
        })
        .transpose()?;
    // The HTML attribute is a render-time snapshot. On a busy runner it can
    // report a completed step as running for one polling cycle, so prefer the
    // UI's live state endpoint and retain the embedded value only for Forgejo
    // versions that do not expose its CSRF metadata.
    let live = fetch_live_run_state(session, &html_s, run_index).await;
    let view = choose_run_state(live, embedded_view).wrap_err_with(|| {
        format!("Unable to determine live or embedded run state from run view HTML ({url}).")
    })?;
    let artifacts: Option<Value> = match initial_artifacts_json {
        Some(s) if !s.trim().is_empty() => Some(
            serde_json::from_str(&s)
                .wrap_err("failed to parse data-initial-artifacts-response json")?,
        ),
        _ => None,
    };

    let meta = RunViewMeta {
        url: url.clone(),
        run_index: html::get_html_attribute_value(&html_s, "data-run-index")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(run_index),
        run_id: html::get_html_attribute_value(&html_s, "data-run-id").and_then(|s| s.parse().ok()),
        job_index: html::get_html_attribute_value(&html_s, "data-job-index")
            .and_then(|s| s.parse().ok()),
        attempt_number: html::get_html_attribute_value(&html_s, "data-attempt-number")
            .and_then(|s| s.parse().ok()),
        actions_url: html::get_html_attribute_value(&html_s, "data-actions-url"),
        id_based_urls: uses_id_based_urls(&html_s),
    };

    Ok(RunViewData {
        meta,
        view,
        artifacts,
    })
}

/// Prefers the live run state, falling back to the page's embedded snapshot
/// when the live request failed (transport error, redirect to an HTML page,
/// a body that is not JSON) or was refused. The live error is reported only
/// when there is no snapshot to use instead.
fn choose_run_state(
    live: eyre::Result<Option<Value>>,
    embedded: Option<Value>,
) -> eyre::Result<Value> {
    match (live, embedded) {
        (Ok(Some(view)), _) => Ok(view),
        (Ok(None), Some(view)) | (Err(_), Some(view)) => Ok(view),
        (Err(err), None) => Err(err),
        (Ok(None), None) => Err(eyre!("the server returned no run state")),
    }
}

/// Whether a run view page belongs to a server that uses id-based Actions URLs.
fn uses_id_based_urls(html_s: &str) -> bool {
    html::get_html_attribute_value(html_s, "data-run-index").is_none()
        && (html::get_html_attribute_value(html_s, "data-run-id").is_some()
            || html::get_html_attribute_value(html_s, "data-actions-view-url").is_some())
}

/// URL the run view page POSTs to for its live state.
fn live_run_state_url(base_url: &str, html_s: &str, run_index: i64) -> Option<String> {
    let attr = |name: &str| html::get_html_attribute_value(html_s, name);
    let number = |name: &str| attr(name).and_then(|s| s.parse::<i64>().ok());

    // Gitea >= 1.27 renders the exact endpoint.
    if let Some(view_url) = attr("data-actions-view-url").filter(|s| !s.is_empty()) {
        return Some(absolute_url(base_url, &view_url));
    }
    let actions_base = absolute_url(base_url, &attr("data-actions-url")?);
    if let Some(index) = number("data-run-index") {
        // Forgejo and Gitea <= 1.25: run index + 0-based job index.
        let job_index = number("data-job-index").unwrap_or(0);
        return Some(format!("{actions_base}/runs/{index}/jobs/{job_index}"));
    }
    // Gitea 1.26: run id + job id, where job id 0 means "the run summary".
    let run_id = number("data-run-id").unwrap_or(run_index);
    match number("data-job-id").filter(|id| *id > 0) {
        Some(job_id) => Some(format!("{actions_base}/runs/{run_id}/jobs/{job_id}")),
        None => Some(format!("{actions_base}/runs/{run_id}")),
    }
}

async fn fetch_live_run_state(
    session: &UiSession,
    html_s: &str,
    run_index: i64,
) -> eyre::Result<Option<Value>> {
    let Some(job_url) = live_run_state_url(session.base_url(), html_s, run_index) else {
        return Ok(None);
    };
    let body = serde_json::json!({ "logCursors": [] });

    // Servers with token-based CSRF protection embed the token in the page;
    // newer Gitea uses header-based protection and renders no token.
    let resp = match html::get_csrf_token_from_html(html_s) {
        Some(csrf_token) => {
            session
                .post_json_response_with_csrf(&job_url, &body, &csrf_token, true)
                .await
        }
        None => session.post_json_response(&job_url, &body, true).await,
    }
    .wrap_err("failed to fetch live run/job json")?;
    if resp.status() != reqwest::StatusCode::OK {
        return Ok(None);
    }
    let text = resp
        .text()
        .await
        .wrap_err("failed to read live run/job json")?;
    let view = serde_json::from_str(&text).wrap_err("failed to parse live run/job json")?;
    Ok(Some(view))
}

/// Path segment addressing job `job_index` of a run in Actions web URLs:
/// the 0-based index on Forgejo and Gitea <= 1.25, the job id on Gitea >= 1.26.
pub fn job_path_segment(view: &RunViewData, job_index: i64) -> eyre::Result<String> {
    if !view.meta.id_based_urls {
        return Ok(job_index.to_string());
    }
    let jobs = get_run_jobs(view.meta.run_index, &view.view)?;
    jobs.iter()
        .find(|job| job.job_index == job_index)
        .and_then(|job| job.id)
        .map(|id| id.to_string())
        .ok_or_else(|| {
            eyre!(
                "Run {} has no job with index {job_index}.",
                view.meta.run_index
            )
        })
}

/// Like [`job_path_segment`], loading the run view first.
pub async fn resolve_job_path_segment(
    session: &UiSession,
    repo: &str,
    run_index: i64,
    job_index: i64,
) -> eyre::Result<String> {
    let view = get_run_view_data(session, repo, run_index)
        .await
        .wrap_err("failed to load run view")?;
    job_path_segment(&view, job_index)
}

pub fn get_run_jobs(run_index: i64, run_view: &Value) -> eyre::Result<Vec<JobInfo>> {
    let Some(jobs) = run_view
        .pointer("/state/run/jobs")
        .and_then(|v| v.as_array())
    else {
        return Err(eyre!("Unable to find jobs at view.state.run.jobs."));
    };

    let mut results = Vec::with_capacity(jobs.len());
    for (idx, j) in jobs.iter().enumerate() {
        results.push(JobInfo {
            run_index,
            job_index: idx as i64,
            id: j.get("id").and_then(|v| v.as_i64()),
            name: j
                .get("name")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            status: j
                .get("status")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            can_rerun: j.get("canRerun").and_then(|v| v.as_bool()),
            duration: j.get("duration").and_then(|v| v.as_i64()),
        });
    }

    Ok(results)
}

pub async fn get_job_view_meta(
    session: &UiSession,
    repo: &str,
    run_index: i64,
    job_index: i64,
) -> eyre::Result<JobViewMeta> {
    let repo_path = repo.trim_matches('/');
    let job = resolve_job_path_segment(session, repo, run_index, job_index).await?;
    let url = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job}",
        session.base_url()
    );

    let resp = session.get_response(&url, true).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre!(
            "Failed to load job view from '{}'. HTTP {}.",
            url,
            resp.status()
        ));
    }
    let html_s = resp.text().await.wrap_err("failed to read response html")?;

    let attempt_number: i64 = match html::get_html_attribute_value(&html_s, "data-attempt-number") {
        Some(s) => s.parse().wrap_err("invalid attempt number")?,
        None => {
            // Older Forgejo versions don't expose attempt numbers in the job view.
            // They still make logs available, typically without an attempt segment.
            ATTEMPT_LOG_RE
                .captures(&html_s)
                .and_then(|caps| caps.name("n").and_then(|m| m.as_str().parse::<i64>().ok()))
                .unwrap_or(1)
        }
    };

    Ok(JobViewMeta {
        url,
        run_index: html::get_html_attribute_value(&html_s, "data-run-index")
            .and_then(|s| s.parse().ok())
            .unwrap_or(run_index),
        run_id: html::get_html_attribute_value(&html_s, "data-run-id").and_then(|s| s.parse().ok()),
        job_index: html::get_html_attribute_value(&html_s, "data-job-index")
            .and_then(|s| s.parse().ok())
            .unwrap_or(job_index),
        attempt_number,
        actions_url: html::get_html_attribute_value(&html_s, "data-actions-url"),
    })
}

pub async fn open_job_logs(
    session: &UiSession,
    repo: &str,
    run_index: i64,
    job_index: i64,
    attempt_number: i64,
) -> eyre::Result<reqwest::Response> {
    let repo_path = repo.trim_matches('/');
    let job = resolve_job_path_segment(session, repo, run_index, job_index).await?;

    // Newer Forgejo versions expose logs under an attempt URL.
    let url_attempt = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job}/attempt/{attempt_number}/logs",
        session.base_url()
    );
    let resp = session.get_response(&url_attempt, true).await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        return Ok(resp);
    }

    // Older Forgejo versions (e.g. 11.x) and Gitea expose logs without attempt numbers.
    let url_legacy = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job}/logs",
        session.base_url()
    );
    let resp = session.get_response(&url_legacy, true).await?;
    let legacy_status = resp.status();
    if legacy_status == reqwest::StatusCode::OK {
        return Ok(resp);
    }

    Err(eyre!(
        "Failed to download logs from '{}' (HTTP {}) and '{}' (HTTP {}).",
        url_attempt,
        status,
        url_legacy,
        legacy_status
    ))
}

pub async fn copy_response_body<W>(mut resp: reqwest::Response, writer: &mut W) -> eyre::Result<u64>
where
    W: AsyncWrite + Unpin,
{
    let mut written = 0_u64;
    while let Some(chunk) = resp
        .chunk()
        .await
        .wrap_err("failed to read response body chunk")?
    {
        writer
            .write_all(&chunk)
            .await
            .wrap_err("failed to write response body chunk")?;
        written += chunk.len() as u64;
    }

    Ok(written)
}

/// The typed error `copy_response_body_with_limit` returns when the body is
/// larger than allowed, so callers can tell it apart from I/O failures.
#[derive(Debug)]
pub struct ResponseBodyLimitExceeded {
    pub received: u64,
    pub max_bytes: u64,
}

impl std::fmt::Display for ResponseBodyLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Response body exceeds limit ({} bytes > {} bytes).",
            self.received, self.max_bytes
        )
    }
}

impl std::error::Error for ResponseBodyLimitExceeded {}

pub async fn copy_response_body_with_limit<W>(
    mut resp: reqwest::Response,
    writer: &mut W,
    max_bytes: u64,
) -> eyre::Result<u64>
where
    W: AsyncWrite + Unpin,
{
    let mut written = 0_u64;
    while let Some(chunk) = resp
        .chunk()
        .await
        .wrap_err("failed to read response body chunk")?
    {
        let next_written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| eyre!("response body is too large to count"))?;
        if next_written > max_bytes {
            return Err(eyre::Report::new(ResponseBodyLimitExceeded {
                received: next_written,
                max_bytes,
            }));
        }

        writer
            .write_all(&chunk)
            .await
            .wrap_err("failed to write response body chunk")?;
        written = next_written;
    }

    Ok(written)
}

pub async fn latest_run_index(
    session: &UiSession,
    repo: &str,
    workflow: Option<&str>,
) -> eyre::Result<i64> {
    let runs = list_runs(session, repo, workflow, 1, 1).await?;
    let latest = runs
        .first()
        .ok_or_else(|| eyre!("No action runs found for {repo}."))?;
    Ok(latest.run_index)
}

/// Forgejo and Gitea cap Actions run-list pages at 50 entries.
const RUN_LOOKUP_PAGE_SIZE: u32 = 50;
/// Upper bound on run-list pages fetched by one [`find_run`] call.
const RUN_LOOKUP_MAX_REQUESTS: usize = 12;

/// Where a run index sits relative to one page of the (newest-first) run list.
#[derive(Debug, PartialEq, Eq)]
enum RunPagePosition {
    /// The run is on this page, at the given position.
    Found(usize),
    /// Every run on this full page is newer: the run is on a later page.
    OnLaterPage,
    /// The page is empty or every run on it is older: the run is on an earlier page.
    OnEarlierPage,
    /// The run would be on this page but is not listed (deleted or never existed).
    Missing,
}

fn locate_run_on_page(runs: &[RunRef], run_index: i64, page_size: u32) -> RunPagePosition {
    if let Some(pos) = runs.iter().position(|run| run.run_index == run_index) {
        return RunPagePosition::Found(pos);
    }
    let (Some(newest), Some(oldest)) = (runs.first(), runs.last()) else {
        return RunPagePosition::OnEarlierPage;
    };
    if oldest.run_index > run_index {
        if runs.len() < page_size as usize {
            // Last page of the list, and the run is older than all of it.
            return RunPagePosition::Missing;
        }
        return RunPagePosition::OnLaterPage;
    }
    if newest.run_index < run_index {
        return RunPagePosition::OnEarlierPage;
    }
    RunPagePosition::Missing
}

/// Bounds the page holding `run_index`, given that the full page `page` ends
/// with the run `oldest` (> `run_index`). At most `oldest - run_index - 1`
/// runs separate the two, whether the numbers are per-repository indexes or
/// instance-wide run ids, so the target is never on a later page.
fn predicted_run_page(page: u32, oldest: i64, run_index: i64, page_size: u32) -> u32 {
    let runs_between = oldest.saturating_sub(run_index).saturating_sub(1).max(0);
    let pages_between =
        u32::try_from(runs_between / i64::from(page_size.max(1))).unwrap_or(u32::MAX);
    page.saturating_add(1).saturating_add(pages_between)
}

/// Estimates the page holding `run_index` from the density of run numbers on
/// the full page `page` (newest first). Per-repository indexes are dense, so
/// this equals [`predicted_run_page`]; instance-wide run ids (Gitea >= 1.26)
/// leave gaps for other repositories' runs, which the density accounts for.
fn estimated_run_page(page: u32, runs: &[RunRef], run_index: i64, page_size: u32) -> u32 {
    let (Some(newest), Some(oldest)) = (runs.first(), runs.last()) else {
        return page.saturating_add(1);
    };
    let span = i128::from(newest.run_index) - i128::from(oldest.run_index);
    let gap = (i128::from(oldest.run_index) - i128::from(run_index) - 1).max(0);
    let listed = i128::try_from(runs.len().saturating_sub(1)).unwrap_or(i128::MAX);
    let runs_between = if span > 0 && listed < span {
        gap * listed / span
    } else {
        gap
    };
    let pages_between =
        u32::try_from(runs_between / i128::from(page_size.max(1))).unwrap_or(u32::MAX);
    page.saturating_add(1).saturating_add(pages_between)
}

/// Next step of a [`find_run`] search.
#[derive(Debug, PartialEq, Eq)]
enum RunSearchStep {
    Found(usize),
    Absent,
    Fetch(u32),
}

/// The page bookkeeping of [`find_run`], separate from the HTTP requests.
#[derive(Debug)]
struct RunSearch {
    run_index: i64,
    /// The size of a full page as the server serves it; an instance
    /// configured with a smaller maximum page size returns fewer runs than
    /// requested.
    page_size: u32,
    /// Pages that may still contain the run: `lowest..=highest`.
    lowest: u32,
    highest: u32,
    page: u32,
    /// Distance from `lowest` of the next probe after reading past the end
    /// of the list; doubles with each such probe.
    gallop: u32,
}

impl RunSearch {
    fn new(run_index: i64) -> Self {
        Self {
            run_index,
            page_size: RUN_LOOKUP_PAGE_SIZE,
            lowest: 1,
            highest: u32::MAX,
            page: 1,
            gallop: 0,
        }
    }

    /// Consumes the runs listed on `self.page` and decides what to do next.
    fn step(&mut self, runs: &[RunRef]) -> RunSearchStep {
        match locate_run_on_page(runs, self.run_index, self.page_size) {
            RunPagePosition::Found(pos) => RunSearchStep::Found(pos),
            RunPagePosition::Missing => RunSearchStep::Absent,
            RunPagePosition::OnLaterPage => {
                let oldest = runs.last().map_or(self.run_index, |run| run.run_index);
                self.lowest = self.page.saturating_add(1);
                self.highest = self.highest.min(predicted_run_page(
                    self.page,
                    oldest,
                    self.run_index,
                    self.page_size,
                ));
                if self.lowest > self.highest {
                    return RunSearchStep::Absent;
                }
                self.page = estimated_run_page(self.page, runs, self.run_index, self.page_size)
                    .clamp(self.lowest, self.highest);
                RunSearchStep::Fetch(self.page)
            }
            RunPagePosition::OnEarlierPage => {
                self.highest = self.page.saturating_sub(1);
                if self.lowest > self.highest {
                    return RunSearchStep::Absent;
                }
                let range = self.highest - self.lowest;
                self.page = if runs.is_empty() {
                    // The estimate went past the end of the list by an unknown
                    // amount (sparse ids after a dense page): search forward
                    // from the last known page with growing steps instead of
                    // bisecting a possibly huge range.
                    let step = self.gallop.min(range);
                    self.gallop = self.gallop.saturating_mul(2).max(1);
                    self.lowest + step
                } else {
                    self.lowest + range / 2
                };
                RunSearchStep::Fetch(self.page)
            }
        }
    }
}

/// Finds a run in the Actions run list by the number in its URL, however old
/// it is.
///
/// The run list (not the run page) carries the authoritative run status, but
/// it is paginated. Per-repository run indexes are allocated sequentially, so
/// the distance to the target predicts its page exactly unless runs were
/// deleted. Instance-wide run ids (Gitea >= 1.26) also grow monotonically but
/// with gaps, so the page is estimated from the density of ids on the page
/// just read. Either way the distance bounds the page from above, and a
/// bounded binary search corrects an overshoot.
pub async fn find_run(
    session: &UiSession,
    repo: &str,
    run_index: i64,
) -> eyre::Result<Option<RunRef>> {
    let mut search = RunSearch::new(run_index);

    for _ in 0..RUN_LOOKUP_MAX_REQUESTS {
        let page = search.page;
        let mut runs = list_runs(session, repo, None, page, RUN_LOOKUP_PAGE_SIZE).await?;
        let short_first_page = page == 1
            && !runs.is_empty()
            && runs.len() < RUN_LOOKUP_PAGE_SIZE as usize
            && runs
                .last()
                .is_some_and(|oldest| oldest.run_index > run_index);
        if short_first_page {
            // Either the whole list fits on one page or the server caps the
            // page size; a second page tells the two apart.
            if list_runs(session, repo, None, 2, RUN_LOOKUP_PAGE_SIZE)
                .await?
                .is_empty()
            {
                return Ok(None);
            }
            search.page_size = u32::try_from(runs.len()).unwrap_or(RUN_LOOKUP_PAGE_SIZE);
        }

        match search.step(&runs) {
            RunSearchStep::Found(pos) => return Ok(Some(runs.swap_remove(pos))),
            RunSearchStep::Absent => return Ok(None),
            RunSearchStep::Fetch(_) => {}
        }
    }

    Ok(None)
}

pub async fn get_run_artifacts(
    session: &UiSession,
    repo: &str,
    run_index: i64,
) -> eyre::Result<Value> {
    let repo_path = repo.trim_matches('/');
    let url = format!(
        "{}/{repo_path}/actions/runs/{run_index}/artifacts",
        session.base_url()
    );

    let resp = session.get_response(&url, true).await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        let run_view = get_run_view_data(session, repo, run_index)
            .await
            .wrap_err("failed to load run view while falling back after artifacts 404")?;
        return Ok(run_view
            .artifacts
            .or_else(|| artifacts_from_live_state(&run_view.view))
            .unwrap_or_else(|| serde_json::json!({ "artifacts": [] })));
    }

    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre!(
            "Failed to fetch artifacts from '{}'. HTTP {}.",
            url,
            resp.status()
        ));
    }
    let text = resp
        .text()
        .await
        .wrap_err("failed to read artifacts json")?;
    let json: Value = serde_json::from_str(&text).wrap_err("failed to parse artifacts json")?;
    Ok(json)
}

/// Gitea's live run state (`POST /runs/{run}`) lists the run's artifacts.
fn artifacts_from_live_state(view: &Value) -> Option<Value> {
    let artifacts = view.get("artifacts")?.as_array()?;
    Some(serde_json::json!({ "artifacts": artifacts }))
}

pub async fn open_artifact(
    session: &UiSession,
    repo: &str,
    run_index: i64,
    artifact_name_or_id: &str,
) -> eyre::Result<reqwest::Response> {
    let repo_path = repo.trim_matches('/');
    let name_or_id = urlencoding::encode(artifact_name_or_id);
    let url = format!(
        "{}/{repo_path}/actions/runs/{run_index}/artifacts/{name_or_id}",
        session.base_url()
    );

    let resp = session.get_response(&url, true).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre!(
            "Failed to download artifact from '{}'. HTTP {}.",
            url,
            resp.status()
        ));
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_absolute_rooted_and_relative_actions_urls() {
        assert_eq!(
            absolute_url("https://forge.example", "https://actions.example/api"),
            "https://actions.example/api"
        );
        assert_eq!(
            absolute_url("https://forge.example", "/api/actions"),
            "https://forge.example/api/actions"
        );
        assert_eq!(
            absolute_url("https://forge.example/", "api/actions"),
            "https://forge.example/api/actions"
        );
    }

    #[test]
    fn repo_href_path_includes_base_url_path() {
        assert_eq!(
            repo_href_path("https://apps.example.com/gitea", "owner/repo"),
            "gitea/owner/repo"
        );
        assert_eq!(
            repo_href_path("https://apps.example.com", "owner/repo"),
            "owner/repo"
        );
    }

    #[test]
    fn repo_href_matches_root_and_subpath_links() {
        assert!(repo_href_matches(
            "owner/repo",
            "owner/repo",
            "https://apps.example.com/gitea"
        ));
        assert!(repo_href_matches(
            "gitea/owner/repo",
            "owner/repo",
            "https://apps.example.com/gitea"
        ));
        assert!(!repo_href_matches(
            "other/owner/repo",
            "owner/repo",
            "https://apps.example.com/gitea"
        ));
    }

    #[test]
    fn live_run_state_url_covers_forgejo_and_gitea_page_layouts() {
        let base = "http://forge.example";
        // Forgejo / Gitea <= 1.25
        let forgejo = r#"<div id="repo-action-view" data-run-index="7" data-job-index="1" data-actions-url="/o/r/actions">"#;
        assert_eq!(
            live_run_state_url(base, forgejo, 7).as_deref(),
            Some("http://forge.example/o/r/actions/runs/7/jobs/1")
        );
        assert!(!uses_id_based_urls(forgejo));
        // Gitea 1.26
        let gitea_126 = r#"<div id="repo-action-view" data-run-id="42" data-job-id="0" data-actions-url="/o/r/actions">"#;
        assert_eq!(
            live_run_state_url(base, gitea_126, 42).as_deref(),
            Some("http://forge.example/o/r/actions/runs/42")
        );
        assert!(uses_id_based_urls(gitea_126));
        let gitea_126_job =
            r#"<div data-run-id="42" data-job-id="99" data-actions-url="/o/r/actions">"#;
        assert_eq!(
            live_run_state_url(base, gitea_126_job, 42).as_deref(),
            Some("http://forge.example/o/r/actions/runs/42/jobs/99")
        );
        // Gitea 1.27
        let gitea_127 = r#"<div id="repo-action-view" data-job-id="0" data-actions-view-url="/o/r/actions/runs/42">"#;
        assert_eq!(
            live_run_state_url(base, gitea_127, 42).as_deref(),
            Some("http://forge.example/o/r/actions/runs/42")
        );
        assert!(uses_id_based_urls(gitea_127));
        assert_eq!(live_run_state_url(base, "<div></div>", 1), None);
    }

    #[test]
    fn job_path_segment_uses_job_ids_only_for_id_based_servers() {
        let view = serde_json::json!({"state": {"run": {"jobs": [{"id": 11}, {"id": 12}]}}});
        let mut data = RunViewData {
            meta: RunViewMeta {
                url: String::new(),
                run_index: 5,
                run_id: None,
                job_index: None,
                attempt_number: None,
                actions_url: None,
                id_based_urls: false,
            },
            view,
            artifacts: None,
        };
        assert_eq!(job_path_segment(&data, 1).unwrap(), "1");
        data.meta.id_based_urls = true;
        assert_eq!(job_path_segment(&data, 1).unwrap(), "12");
        assert!(job_path_segment(&data, 2).is_err());
    }

    #[test]
    fn artifacts_are_read_from_gitea_live_state() {
        let view = serde_json::json!({"artifacts": [{"name": "a", "size": 3}], "state": {}});
        assert_eq!(
            artifacts_from_live_state(&view),
            Some(serde_json::json!({"artifacts": [{"name": "a", "size": 3}]}))
        );
        assert_eq!(
            artifacts_from_live_state(&serde_json::json!({"state": {}})),
            None
        );
    }

    #[test]
    fn absolute_url_resolves_root_relative_links_against_origin() {
        assert_eq!(
            absolute_url("https://apps.example.com/gitea", "/gitea/-/actions"),
            "https://apps.example.com/gitea/-/actions"
        );
        assert_eq!(
            absolute_url("https://apps.example.com/gitea", "-/actions"),
            "https://apps.example.com/gitea/-/actions"
        );
    }

    fn runs(indexes: impl IntoIterator<Item = i64>) -> Vec<RunRef> {
        indexes
            .into_iter()
            .map(|run_index| RunRef {
                run_index,
                url: format!("https://forge.example/o/r/actions/runs/{run_index}"),
                status: Some("Success".to_string()),
                branch: None,
                created_at: None,
            })
            .collect()
    }

    #[test]
    fn locate_run_on_page_finds_listed_runs() {
        assert_eq!(
            locate_run_on_page(&runs([12, 11, 10]), 11, 3),
            RunPagePosition::Found(1)
        );
    }

    #[test]
    fn locate_run_on_page_continues_past_a_full_page_of_newer_runs() {
        assert_eq!(
            locate_run_on_page(&runs((101..=150).rev()), 3, 50),
            RunPagePosition::OnLaterPage
        );
        // A short page is the end of the list, so an older run is not listed.
        assert_eq!(
            locate_run_on_page(&runs([20, 19]), 3, 50),
            RunPagePosition::Missing
        );
    }

    #[test]
    fn locate_run_on_page_goes_back_from_older_or_empty_pages() {
        assert_eq!(
            locate_run_on_page(&runs([5, 4, 3]), 9, 3),
            RunPagePosition::OnEarlierPage
        );
        assert_eq!(
            locate_run_on_page(&runs([]), 9, 3),
            RunPagePosition::OnEarlierPage
        );
    }

    #[test]
    fn predicted_run_page_jumps_directly_to_older_runs() {
        // Page 1 holds runs 150..=101, page 2 holds 100..=51, page 3 holds 50..=1.
        assert_eq!(predicted_run_page(1, 101, 100, 50), 2);
        assert_eq!(predicted_run_page(1, 101, 51, 50), 2);
        assert_eq!(predicted_run_page(1, 101, 50, 50), 3);
        assert_eq!(predicted_run_page(1, 101, 1, 50), 3);
        assert_eq!(predicted_run_page(2, 51, 1, 50), 3);
    }

    /// Runs [`RunSearch`] against a newest-first run list served in pages of
    /// `page_size`, returning the found run number and the pages requested.
    fn simulate_find_run(all: &[i64], page_size: usize, target: i64) -> (Option<i64>, Vec<u32>) {
        let mut search = RunSearch::new(target);
        search.page_size = u32::try_from(page_size).unwrap();
        let mut fetched = Vec::new();
        for _ in 0..RUN_LOOKUP_MAX_REQUESTS {
            let page = search.page;
            fetched.push(page);
            let start = (page as usize - 1).saturating_mul(page_size);
            let listed = runs(all.iter().copied().skip(start).take(page_size));
            match search.step(&listed) {
                RunSearchStep::Found(pos) => return (Some(listed[pos].run_index), fetched),
                RunSearchStep::Absent => return (None, fetched),
                RunSearchStep::Fetch(next) => assert_eq!(next, search.page),
            }
        }
        (None, fetched)
    }

    #[test]
    fn find_run_jumps_straight_to_the_page_of_a_dense_index() {
        let all: Vec<i64> = (1..=150).rev().collect();
        assert_eq!(simulate_find_run(&all, 50, 120), (Some(120), vec![1]));
        assert_eq!(simulate_find_run(&all, 50, 75), (Some(75), vec![1, 2]));
        assert_eq!(simulate_find_run(&all, 50, 1), (Some(1), vec![1, 3]));
    }

    #[test]
    fn find_run_stops_once_the_gap_between_pages_is_known() {
        // Runs 200..=151 were deleted; 175 cannot be listed anywhere.
        let all: Vec<i64> = (251..=300)
            .rev()
            .chain((201..=250).rev())
            .chain((101..=150).rev())
            .collect();
        let (found, fetched) = simulate_find_run(&all, 50, 175);
        assert_eq!(found, None);
        assert_eq!(fetched, vec![1, 3, 2]);
    }

    #[test]
    fn find_run_finds_sparse_instance_wide_run_ids_on_later_pages() {
        // Gitea >= 1.26: 60 runs of this repository, with ids spread over
        // 600000 runs of the whole instance.
        let all: Vec<i64> = (0..60).map(|i| 900_000 - i * 10_000).collect();
        let target = all[55];
        let (found, fetched) = simulate_find_run(&all, 50, target);
        assert_eq!(found, Some(target));
        assert_eq!(fetched, vec![1, 2]);

        // A burst of ids on the first page still finds the run within the
        // request budget.
        let all: Vec<i64> = (900_001..=900_050)
            .rev()
            .chain((0..60).map(|i| 300_000 - i * 1_000))
            .collect();
        for target in [300_000, all[80], *all.last().unwrap()] {
            let (found, fetched) = simulate_find_run(&all, 50, target);
            assert_eq!(found, Some(target), "pages requested: {fetched:?}");
        }
        let (found, _) = simulate_find_run(&all, 50, 300_001);
        assert_eq!(found, None);
    }

    #[test]
    fn run_state_falls_back_to_the_embedded_snapshot_when_the_live_post_fails() {
        let live = serde_json::json!({ "source": "live" });
        let embedded = serde_json::json!({ "source": "embedded" });
        assert_eq!(
            choose_run_state(Ok(Some(live.clone())), Some(embedded.clone())).unwrap(),
            live
        );
        assert_eq!(
            choose_run_state(Ok(None), Some(embedded.clone())).unwrap(),
            embedded
        );
        // e.g. the POST was redirected to an HTML page.
        assert_eq!(
            choose_run_state(
                Err(eyre!("failed to parse live run/job json")),
                Some(embedded.clone())
            )
            .unwrap(),
            embedded
        );
        let err =
            choose_run_state(Err(eyre!("failed to parse live run/job json")), None).unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse live run/job json"));
        assert!(choose_run_state(Ok(None), None).is_err());
    }

    #[test]
    fn locate_run_on_page_reports_a_deleted_run_inside_the_page_range() {
        assert_eq!(
            locate_run_on_page(&runs([12, 10, 9]), 11, 3),
            RunPagePosition::Missing
        );
    }
}
