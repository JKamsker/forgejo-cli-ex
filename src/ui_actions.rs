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
    let view = match fetch_live_run_state(session, &html_s, run_index).await? {
        Some(view) => view,
        None => embedded_view.ok_or_else(|| {
            eyre!("Unable to determine live or embedded run state from run view HTML ({url}).")
        })?,
    };
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
    };

    Ok(RunViewData {
        meta,
        view,
        artifacts,
    })
}

async fn fetch_live_run_state(
    session: &UiSession,
    html_s: &str,
    run_index: i64,
) -> eyre::Result<Option<Value>> {
    let Some(csrf_token) = html::get_csrf_token_from_html(html_s) else {
        return Ok(None);
    };
    let Some(actions_url) = html::get_html_attribute_value(html_s, "data-actions-url") else {
        return Ok(None);
    };

    let effective_run_index = html::get_html_attribute_value(html_s, "data-run-index")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(run_index);
    let job_index = html::get_html_attribute_value(html_s, "data-job-index")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let actions_base = absolute_url(session.base_url(), &actions_url);
    let job_url = format!("{actions_base}/runs/{effective_run_index}/jobs/{job_index}");
    let body = serde_json::json!({ "logCursors": [] });

    let resp = session
        .post_json_response_with_csrf(&job_url, &body, &csrf_token, true)
        .await
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
    let url = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}",
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

    // Newer Forgejo versions expose logs under an attempt URL.
    let url_attempt = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/attempt/{attempt_number}/logs",
        session.base_url()
    );
    let resp = session.get_response(&url_attempt, true).await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        return Ok(resp);
    }

    // Older Forgejo versions (e.g. 11.x) expose logs without attempt numbers.
    let url_legacy = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/logs",
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
            return Err(eyre!(
                "Response body exceeds limit ({next_written} bytes > {max_bytes} bytes)."
            ));
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

/// Predicts the page holding `run_index`, given that `page` ends with the run
/// `oldest` (> `run_index`). At most `oldest - run_index - 1` runs separate the
/// two, so the prediction is exact without deletions and never too early.
fn predicted_run_page(page: u32, oldest: i64, run_index: i64, page_size: u32) -> u32 {
    let runs_between = oldest.saturating_sub(run_index).saturating_sub(1).max(0);
    let pages_between =
        u32::try_from(runs_between / i64::from(page_size.max(1))).unwrap_or(u32::MAX);
    page.saturating_add(1).saturating_add(pages_between)
}

/// Finds a run in the Actions run list by its index, however old it is.
///
/// The run list (not the run page) carries the authoritative run status, but
/// it is paginated. Run indexes are allocated sequentially per repository, so
/// the distance to the target index predicts its page exactly unless runs were
/// deleted; in that case the prediction overshoots and is corrected by a
/// bounded binary search.
pub async fn find_run(
    session: &UiSession,
    repo: &str,
    run_index: i64,
) -> eyre::Result<Option<RunRef>> {
    let page_size = RUN_LOOKUP_PAGE_SIZE;
    // Pages that may still contain the run: lowest..=highest.
    let mut lowest: u32 = 1;
    let mut highest: u32 = u32::MAX;
    let mut page: u32 = 1;

    for _ in 0..RUN_LOOKUP_MAX_REQUESTS {
        let mut runs = list_runs(session, repo, None, page, page_size).await?;
        match locate_run_on_page(&runs, run_index, page_size) {
            RunPagePosition::Found(pos) => return Ok(Some(runs.swap_remove(pos))),
            RunPagePosition::Missing => return Ok(None),
            RunPagePosition::OnLaterPage => {
                lowest = page.saturating_add(1);
                let oldest = runs.last().map_or(run_index, |run| run.run_index);
                page = predicted_run_page(page, oldest, run_index, page_size).min(highest);
            }
            RunPagePosition::OnEarlierPage => {
                highest = page.saturating_sub(1);
                if lowest > highest {
                    return Ok(None);
                }
                page = lowest + (highest - lowest) / 2;
            }
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

    #[test]
    fn locate_run_on_page_reports_a_deleted_run_inside_the_page_range() {
        assert_eq!(
            locate_run_on_page(&runs([12, 10, 9]), 11, 3),
            RunPagePosition::Missing
        );
    }
}
