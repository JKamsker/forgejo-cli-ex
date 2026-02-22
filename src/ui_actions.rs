use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use eyre::{eyre, Context};
use regex::Regex;
use serde_json::Value;

use crate::{html, session::UiSession};

/// Maximum bytes before a run-href match to search for its status tooltip.
const STATUS_LOOKBACK_BYTES: usize = 800;
/// Maximum bytes after a run-href match to search for branch/created_at metadata.
const RUN_BLOCK_LOOKAHEAD_BYTES: usize = 8_000;

static RUN_HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"href="/(?P<repo>[^"]+)/actions/runs/(?P<idx>\d+)""#)
        .expect("RUN_HREF_RE regex must be valid")
});
static STATUS_TOOLTIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"data-tooltip-content="(?P<status>Success|Failure|Running|Waiting|Canceled|Cancelled|Skipped|Blocked)""#,
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

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
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

    let re = Regex::new(r#"href="\?workflow=([^"&]+)"#).wrap_err("failed to build regex")?;
    let mut names = BTreeSet::new();
    for caps in re.captures_iter(&html_s) {
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

    let run_href_prefix = format!("href=\"/{repo_path}/actions/runs/");

    let mut seen = HashSet::new();
    let mut runs = Vec::new();
    for caps in RUN_HREF_RE.captures_iter(&html_s) {
        let m = caps
            .get(0)
            .ok_or_else(|| eyre!("run regex capture missing match"))?;
        let repo_m = caps.name("repo").map(|m| m.as_str()).unwrap_or_default();
        if repo_m != repo_path {
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
        let after_end = html_s[m.end()..max_after_end]
            .find(&run_href_prefix)
            .map(|i| m.end() + i)
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

    let view: Value = if let Some(job_json) = initial_job_json {
        serde_json::from_str(&job_json)
            .wrap_err("failed to parse data-initial-post-response json")?
    } else {
        // Older Forgejo versions (e.g. 11.x) don't embed the initial JSON response and instead
        // fetch it via a CSRF-protected UI endpoint.
        let csrf_token = html::get_csrf_token_from_html(&html_s)
            .ok_or_else(|| eyre!("Unable to determine CSRF token in run view HTML ({url})."))?;
        let actions_url =
            html::get_html_attribute_value(&html_s, "data-actions-url").ok_or_else(|| {
                eyre!("Unable to determine data-actions-url in run view HTML ({url}).")
            })?;

        let effective_run_index = html::get_html_attribute_value(&html_s, "data-run-index")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(run_index);
        let job_index = html::get_html_attribute_value(&html_s, "data-job-index")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        let actions_base =
            if actions_url.starts_with("http://") || actions_url.starts_with("https://") {
                actions_url
            } else if actions_url.starts_with('/') {
                format!("{}{}", session.base_url(), actions_url)
            } else {
                format!(
                    "{}/{}",
                    session.base_url().trim_end_matches('/'),
                    actions_url
                )
            };
        let job_url = format!("{actions_base}/runs/{effective_run_index}/jobs/{job_index}");
        let body = serde_json::json!({ "logCursors": [] });

        let resp = session
            .post_json_response_with_csrf(&job_url, &body, &csrf_token, true)
            .await
            .wrap_err("failed to fetch run/job json")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status != reqwest::StatusCode::OK {
            return Err(eyre!(
                "Failed to load run/job state from '{}'. HTTP {} body={}",
                job_url,
                status,
                text
            ));
        }

        serde_json::from_str(&text).wrap_err("failed to parse run/job json")?
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
            let re = Regex::new(r#"/attempt/(?P<n>\d+)/logs"#).ok();
            re.and_then(|re| {
                re.captures(&html_s)
                    .and_then(|caps| caps.name("n").and_then(|m| m.as_str().parse::<i64>().ok()))
            })
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

pub async fn download_job_logs(
    session: &UiSession,
    repo: &str,
    run_index: i64,
    job_index: i64,
    attempt_number: i64,
) -> eyre::Result<Vec<u8>> {
    let repo_path = repo.trim_matches('/');

    // Newer Forgejo versions expose logs under an attempt URL.
    let url_attempt = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/attempt/{attempt_number}/logs",
        session.base_url()
    );
    let resp = session.get_response(&url_attempt, true).await?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        let bytes = resp.bytes().await.wrap_err("failed to read log bytes")?;
        return Ok(bytes.to_vec());
    }

    // Older Forgejo versions (e.g. 11.x) expose logs without attempt numbers.
    let url_legacy = format!(
        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/logs",
        session.base_url()
    );
    let resp = session.get_response(&url_legacy, true).await?;
    let legacy_status = resp.status();
    if legacy_status == reqwest::StatusCode::OK {
        let bytes = resp.bytes().await.wrap_err("failed to read log bytes")?;
        return Ok(bytes.to_vec());
    }

    Err(eyre!(
        "Failed to download logs from '{}' (HTTP {}) and '{}' (HTTP {}).",
        url_attempt,
        status,
        url_legacy,
        legacy_status
    ))
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

pub async fn download_artifact(
    session: &UiSession,
    repo: &str,
    run_index: i64,
    artifact_name_or_id: &str,
) -> eyre::Result<Vec<u8>> {
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
    let bytes = resp
        .bytes()
        .await
        .wrap_err("failed to read artifact bytes")?;
    Ok(bytes.to_vec())
}
