//! `actions workflows`, `actions runs` (filters, --latest, --json),
//! `actions jobs`, and `actions wait` exit codes on the push-triggered runs.

use eyre::{ensure, eyre, Result};
use serde_json::Value;

use super::common::{self, status_of};
use super::Ctx;

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let ci = ctx.repo("ci-artifacts");
    let mixed = ctx.repo("ci-mixed");

    ctx.step("push-triggered runs appear and finish");
    let ci_run = common::wait_new_run(ctx, &ci, "ci.yml", None).await?;
    let mixed_run = common::wait_new_run(ctx, &mixed, "mixed.yml", None).await?;
    let s = common::wait_run_done(ctx, &ci, ci_run).await?;
    ensure!(s == "success", "ci run {ci_run} ended {s}");
    let s = common::wait_run_done(ctx, &mixed, mixed_run).await?;
    ensure!(s == "failure", "mixed run {mixed_run} ended {s}");

    ctx.step("workflows --json");
    let v = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["workflows", "--json"]))
        .json()
        .await?;
    ensure!(
        v["workflows"]
            .as_array()
            .is_some_and(|w| w.iter().any(|x| x == "ci.yml")),
        "workflows: {v}"
    );
    let v = ctx
        .fj
        .cmd(&ctx.actions(&mixed, &["workflows"]))
        .ok()
        .await?;
    ensure!(
        v.stdout.lines().any(|l| l.trim() == "mixed.yml"),
        "workflows text:\n{}",
        v.stdout
    );

    ctx.step("runs --json / --latest / --limit");
    let v = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["runs", "--json", "--limit", "5"]))
        .json()
        .await?;
    let run = find_run(&v, ci_run)?;
    ensure!(status_of(run) == "success", "run: {run}");
    ensure!(run["branch"] == "main", "branch: {run}");
    ensure!(
        run["url"]
            .as_str()
            .is_some_and(|u| u.ends_with(&format!("/{ci}/actions/runs/{ci_run}"))),
        "url: {run}"
    );
    ensure!(run["createdAt"].is_string(), "createdAt: {run}");
    let latest = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["runs", "--latest", "--json"]))
        .json()
        .await?;
    ensure!(
        latest["runs"].as_array().map(Vec::len) == Some(1),
        "--latest: {latest}"
    );
    ensure!(
        latest["limit"] == 1 && latest["page"] == 1,
        "--latest paging: {latest}"
    );

    ctx.step("runs --status filters");
    let failures = ctx
        .fj
        .cmd(&ctx.actions(&mixed, &["runs", "--status", "failure", "--json"]))
        .json()
        .await?;
    ensure!(
        find_run(&failures, mixed_run).is_ok(),
        "failure filter misses run: {failures}"
    );
    let successes = ctx
        .fj
        .cmd(&ctx.actions(&mixed, &["runs", "--status", "success", "--json"]))
        .json()
        .await?;
    ensure!(
        find_run(&successes, mixed_run).is_err(),
        "success filter includes failed run: {successes}"
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(&mixed, &["runs", "--status", "bogus"]))
        .fails()
        .await?;
    ensure!(!out.stderr.is_empty(), "invalid status should explain");

    ctx.step("runs --workflow filter and table output");
    let v = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["runs", "--workflow", "ci.yml", "--json"]))
        .json()
        .await?;
    ensure!(find_run(&v, ci_run).is_ok(), "workflow filter: {v}");
    let v = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["runs", "--workflow", "does-not-exist.yml", "--json"]))
        .json()
        .await?;
    ensure!(
        v["runs"].as_array().is_some_and(|r| r.is_empty()),
        "unknown workflow filter: {v}"
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["runs", "--header", "--show-url"]))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("RunIndex") && out.stdout.contains(&format!("/actions/runs/{ci_run}")),
        "runs table:\n{}",
        out.stdout
    );

    ctx.step("jobs --json / --latest --workflow (table)");
    let ci_jobs = common::jobs(ctx, &ci, ci_run).await?;
    ensure!(
        ci_jobs.len() == 1 && ci_jobs[0]["name"] == "build",
        "ci jobs: {ci_jobs:?}"
    );
    ensure!(status_of(&ci_jobs[0]) == "success", "ci job: {ci_jobs:?}");
    let mixed_jobs = common::jobs(ctx, &mixed, mixed_run).await?;
    let pass_idx = common::job_index(&mixed_jobs, "pass")?;
    let fail_idx = common::job_index(&mixed_jobs, "fail")?;
    ensure!(
        job_status(&mixed_jobs, pass_idx) == "success",
        "pass job: {mixed_jobs:?}"
    );
    ensure!(
        job_status(&mixed_jobs, fail_idx) == "failure",
        "fail job: {mixed_jobs:?}"
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &["jobs", "--latest", "--workflow", "mixed.yml", "--header"],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("JobIndex")
            && out.stdout.contains("pass")
            && out.stdout.contains("fail"),
        "jobs table:\n{}",
        out.stdout
    );

    ctx.step("wait: success exits 0");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "wait",
                "--run-index",
                &ci_run.to_string(),
                "--json",
                "--timeout",
                "2m",
            ],
        ))
        .json()
        .await?;
    ensure!(
        v["status"] == "success" && v["runIndex"] == ci_run,
        "wait json: {v}"
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "wait",
                "--latest",
                "--workflow",
                "ci.yml",
                "--timeout",
                "2m",
            ],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("completed with status 'success'"),
        "wait text:\n{}",
        out.stdout
    );

    ctx.step("wait: failure exits non-zero");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &["wait", "--run-index", &mixed_run.to_string(), "--json"],
        ))
        .fails()
        .await?;
    let v = out.json()?;
    ensure!(v["status"] == "failure", "failed wait json: {v}");
    ensure!(
        out.stderr.contains("failure"),
        "failed wait stderr:\n{}",
        out.stderr
    );

    ctx.step("wait --job-index: passing job exits 0, failing job exits non-zero");
    ctx.fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "wait",
                "--run-index",
                &mixed_run.to_string(),
                "--job-index",
                &pass_idx.to_string(),
            ],
        ))
        .ok()
        .await?;
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "wait",
                "--run-index",
                &mixed_run.to_string(),
                "--job-index",
                &fail_idx.to_string(),
            ],
        ))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains(&format!("job {fail_idx}")),
        "job wait stderr:\n{}",
        out.stderr
    );
    Ok(())
}

fn find_run(v: &Value, run_index: i64) -> Result<&Value> {
    v["runs"]
        .as_array()
        .and_then(|runs| runs.iter().find(|r| r["runIndex"] == run_index))
        .ok_or_else(|| eyre!("run {run_index} not in {v}"))
}

fn job_status(jobs: &[Value], idx: i64) -> String {
    jobs.iter()
        .find(|j| j["jobIndex"] == idx)
        .map(status_of)
        .unwrap_or_default()
}
