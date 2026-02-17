use std::collections::{BTreeSet, HashSet};

use eyre::{eyre, Context};
use regex::Regex;
use serde_json::Value;

use crate::{html, session::UiSession};

#[derive(Clone, Debug)]
pub struct RunRef {
    pub run_index: i64,
    pub url: String,
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
) -> eyre::Result<Vec<String>> {
    let repo_path = repo.trim_matches('/');
    let url = format!(
        "{}/{repo_path}/actions?page={page}&list_inner=true",
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

    let re = Regex::new(&format!(
        r#"/{}/actions/runs/(?P<idx>\d+)"#,
        regex::escape(repo_path)
    ))
    .wrap_err("failed to build regex")?;

    let mut seen = HashSet::new();
    let mut runs = Vec::new();
    for caps in re.captures_iter(&html_s) {
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
        let run_url = format!(
            "{}/{repo_path}/actions/runs/{run_index}",
            session.base_url()
        );
        runs.push(RunRef {
            run_index,
            url: run_url,
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
        .ok_or_else(|| {
            eyre!("Unable to find data-initial-post-response in run view HTML ({url}).")
        })?;
    let initial_artifacts_json =
        html::get_html_attribute_value(&html_s, "data-initial-artifacts-response");

    let view: Value = serde_json::from_str(&initial_job_json)
        .wrap_err("failed to parse data-initial-post-response json")?;
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

    let attempt_attr = html::get_html_attribute_value(&html_s, "data-attempt-number")
        .ok_or_else(|| eyre!("Unable to determine attempt number for run '{run_index}' job '{job_index}' from '{url}'."))?;
    let attempt_number: i64 = attempt_attr.parse().wrap_err("invalid attempt number")?;

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

pub async fn latest_run_index(session: &UiSession, repo: &str) -> eyre::Result<i64> {
    let runs = list_runs(session, repo, None, 1, 1).await?;
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
