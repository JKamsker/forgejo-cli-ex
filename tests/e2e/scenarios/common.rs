//! Polling helpers built on `fj-ex actions runs/jobs --json`.

use std::time::Duration;

use eyre::{eyre, Result};
use serde_json::Value;

use super::Ctx;
use crate::e2e::harness::util;

pub const RUN_APPEAR_TIMEOUT: Duration = Duration::from_secs(120);
pub const RUN_FINISH_TIMEOUT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(2);

/// Lower-cased status with `cancelled` folded into `canceled`.
pub fn norm_status(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    if s == "cancelled" {
        "canceled".into()
    } else {
        s
    }
}

pub fn is_terminal(status: &str) -> bool {
    matches!(
        norm_status(status).as_str(),
        "success" | "failure" | "canceled" | "skipped" | "blocked"
    )
}

pub fn status_of(v: &Value) -> String {
    v["status"].as_str().map(norm_status).unwrap_or_default()
}

/// Runs of `repo` (newest first), optionally filtered by workflow file.
pub async fn runs(ctx: &Ctx<'_>, repo: &str, workflow: Option<&str>) -> Result<Vec<Value>> {
    let mut args = vec!["runs", "--json", "--limit", "50"];
    if let Some(w) = workflow {
        args.extend(["--workflow", w]);
    }
    let v = ctx.fj.cmd(&ctx.actions(repo, &args)).json().await?;
    Ok(v["runs"].as_array().cloned().unwrap_or_default())
}

pub async fn latest_run_index(ctx: &Ctx<'_>, repo: &str, workflow: &str) -> Result<Option<i64>> {
    Ok(runs(ctx, repo, Some(workflow))
        .await?
        .iter()
        .filter_map(|r| r["runIndex"].as_i64())
        .max())
}

/// Wait for a run of `workflow` with an index greater than `after` to appear.
pub async fn wait_new_run(
    ctx: &Ctx<'_>,
    repo: &str,
    workflow: &str,
    after: Option<i64>,
) -> Result<i64> {
    util::poll(
        &format!("a new {workflow} run in {repo}"),
        RUN_APPEAR_TIMEOUT,
        POLL,
        || async move {
            Ok(runs(ctx, repo, Some(workflow))
                .await?
                .iter()
                .filter_map(|r| r["runIndex"].as_i64())
                .filter(|i| after.is_none_or(|a| *i > a))
                .max())
        },
    )
    .await
}

/// Status of one run as reported by the runs list.
pub async fn run_status(ctx: &Ctx<'_>, repo: &str, run_index: i64) -> Result<Option<String>> {
    Ok(runs(ctx, repo, None)
        .await?
        .iter()
        .find(|r| r["runIndex"].as_i64() == Some(run_index))
        .map(status_of))
}

/// Wait until the runs list reports `pred(status)` for the run.
pub async fn wait_run_status(
    ctx: &Ctx<'_>,
    repo: &str,
    run_index: i64,
    what: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool + Copy,
) -> Result<String> {
    util::poll(
        &format!("run {run_index} of {repo} to be {what}"),
        timeout,
        POLL,
        || async move { Ok(run_status(ctx, repo, run_index).await?.filter(|s| pred(s))) },
    )
    .await
}

/// Wait for a terminal state and return it.
pub async fn wait_run_done(ctx: &Ctx<'_>, repo: &str, run_index: i64) -> Result<String> {
    wait_run_status(
        ctx,
        repo,
        run_index,
        "finished",
        RUN_FINISH_TIMEOUT,
        is_terminal,
    )
    .await
}

pub async fn jobs(ctx: &Ctx<'_>, repo: &str, run_index: i64) -> Result<Vec<Value>> {
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            repo,
            &["jobs", "--run-index", &run_index.to_string(), "--json"],
        ))
        .json()
        .await?;
    Ok(v["jobs"].as_array().cloned().unwrap_or_default())
}

pub fn job_index(jobs: &[Value], name: &str) -> Result<i64> {
    jobs.iter()
        .find(|j| j["name"].as_str() == Some(name))
        .and_then(|j| j["jobIndex"].as_i64())
        .ok_or_else(|| eyre!("no job named {name} in {}", Value::Array(jobs.to_vec())))
}

/// Wait until every job of the run satisfies `pred(job)`.
pub async fn wait_jobs(
    ctx: &Ctx<'_>,
    repo: &str,
    run_index: i64,
    what: &str,
    pred: impl Fn(&[Value]) -> bool + Copy,
) -> Result<Vec<Value>> {
    util::poll(
        &format!("jobs of run {run_index} in {repo} to be {what}"),
        RUN_FINISH_TIMEOUT,
        POLL,
        || async move {
            let j = jobs(ctx, repo, run_index).await?;
            Ok((!j.is_empty() && pred(&j)).then_some(j))
        },
    )
    .await
}
