//! `actions cancel` and `actions rerun` (full, single job, `--failed-only`),
//! plus `actions wait --timeout` and the canceled exit code.

use std::time::Duration;

use eyre::{ensure, eyre, Result};

use super::common::{self, status_of};
use super::Ctx;

pub async fn cancel(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("slow-job");
    let before = common::latest_run_index(ctx, &repo, "slow.yml").await?;
    ctx.step("dispatch a long slow.yml run");
    ctx.fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "slow.yml",
                "--input",
                "seconds=600",
            ],
        ))
        .ok()
        .await?;
    let run = common::wait_new_run(ctx, &repo, "slow.yml", before).await?;
    let run_s = run.to_string();
    common::wait_jobs(ctx, &repo, run, "running", |jobs| {
        status_of(&jobs[0]) == "running"
    })
    .await?;

    ctx.step("wait --timeout on a running run exits non-zero");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "wait",
                "--run-index",
                &run_s,
                "--timeout",
                "3s",
                "--interval",
                "1",
            ],
        ))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("timeout"),
        "wait --timeout stderr:\n{}",
        out.stderr
    );

    ctx.step("cancel --dry-run --json");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["cancel", "--run-index", &run_s, "--dry-run", "--json"],
        ))
        .json()
        .await?;
    ensure!(
        v["dryRun"] == true && v["url"].as_str().is_some_and(|u| u.ends_with("/cancel")),
        "{v}"
    );
    let status = common::run_status(ctx, &repo, run)
        .await?
        .unwrap_or_default();
    ensure!(status == "running", "dry run changed the run: {status}");

    ctx.step("cancel --latest --workflow");
    let out = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["cancel", "--latest", "--workflow", "slow.yml"]))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains(&format!("Canceled run #{run}")),
        "cancel output:\n{}",
        out.stdout
    );
    common::wait_run_status(ctx, &repo, run, "canceled", Duration::from_secs(120), |s| {
        s == "canceled"
    })
    .await?;

    ctx.step("wait on a canceled run exits non-zero");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["wait", "--run-index", &run_s, "--json", "--timeout", "1m"],
        ))
        .fails()
        .await?;
    let v = out.json()?;
    ensure!(
        v["status"] == "canceled" || v["status"] == "cancelled",
        "canceled wait json: {v}"
    );
    let jobs = common::jobs(ctx, &repo, run).await?;
    ensure!(status_of(&jobs[0]) == "canceled", "canceled job: {jobs:?}");
    Ok(())
}

pub async fn rerun(ctx: &Ctx<'_>) -> Result<()> {
    let mixed = ctx.repo("ci-mixed");
    let ci = ctx.repo("ci-artifacts");
    let mixed_run = common::latest_run_index(ctx, &mixed, "mixed.yml")
        .await?
        .ok_or_else(|| eyre!("no mixed run"))?;
    let ci_run = common::latest_run_index(ctx, &ci, "ci.yml")
        .await?
        .ok_or_else(|| eyre!("no ci run"))?;
    let mixed_s = mixed_run.to_string();
    let jobs = common::jobs(ctx, &mixed, mixed_run).await?;
    let pass_idx = common::job_index(&jobs, "pass")?;
    let fail_idx = common::job_index(&jobs, "fail")?;

    ctx.step("rerun --failed-only --dry-run lists only the failed job");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "rerun",
                "--run-index",
                &mixed_s,
                "--failed-only",
                "--dry-run",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(
        v["dryRun"] == true && v["jobIndexes"] == serde_json::json!([fail_idx]),
        "dry run: {v}"
    );

    ctx.step("set FJEX_MIXED_SHOULD_PASS=true, then rerun --failed-only");
    ctx.fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "variables",
                "set",
                "--name",
                "FJEX_MIXED_SHOULD_PASS",
                "--value-stdin",
            ],
        ))
        .stdin("true\n")
        .ok()
        .await?;
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &["rerun", "--run-index", &mixed_s, "--failed-only", "--json"],
        ))
        .json()
        .await?;
    ensure!(
        v["requested"] == true && v["jobIndexes"] == serde_json::json!([fail_idx]),
        "rerun: {v}"
    );
    let jobs = common::wait_jobs(ctx, &mixed, mixed_run, "green after the rerun", |jobs| {
        jobs.iter().all(|j| status_of(j) == "success")
    })
    .await?;
    ensure!(jobs.len() == 2, "jobs after rerun: {jobs:?}");
    ensure!(
        jobs.iter()
            .any(|j| j["jobIndex"] == pass_idx && status_of(j) == "success"),
        "pass job changed: {jobs:?}"
    );
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &["wait", "--run-index", &mixed_s, "--json", "--timeout", "3m"],
        ))
        .timeout(Duration::from_secs(200))
        .json()
        .await?;
    ensure!(v["status"] == "success", "wait after rerun: {v}");
    let log = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "logs",
                "job",
                "--run-index",
                &mixed_s,
                "--job-index",
                &fail_idx.to_string(),
            ],
        ))
        .ok()
        .await?;
    ensure!(
        log.stdout.contains("fail job passes now"),
        "rerun log:\n{}",
        log.stdout
    );

    ctx.step("rerun --failed-only with nothing failed is a no-op");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "rerun",
                "--latest",
                "--workflow",
                "mixed.yml",
                "--failed-only",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(v["requested"] == false, "no-op rerun: {v}");

    ctx.step("rerun --job-index (dry run and real)");
    let ci_s = ci_run.to_string();
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "rerun",
                "--run-index",
                &ci_s,
                "--job-index",
                "0",
                "--dry-run",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(
        v["url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/rerun") && u.contains("/jobs/")),
        "job rerun url: {v}"
    );
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &["rerun", "--run-index", &ci_s, "--job-index", "0", "--json"],
        ))
        .json()
        .await?;
    ensure!(v["requested"] == true, "job rerun: {v}");
    rerun_settles(ctx, &ci, ci_run).await?;

    ctx.step("rerun (whole run)");
    let out = ctx
        .fj
        .cmd(&ctx.actions(&ci, &["rerun", "--run-index", &ci_s]))
        .ok()
        .await?;
    ensure!(
        out.stdout
            .contains(&format!("Rerun requested for run #{ci_run}")),
        "rerun output:\n{}",
        out.stdout
    );
    rerun_settles(ctx, &ci, ci_run).await?;
    Ok(())
}

/// After a rerun request the run must leave its old terminal state and reach success again.
async fn rerun_settles(ctx: &Ctx<'_>, repo: &str, run: i64) -> Result<()> {
    common::wait_jobs(ctx, repo, run, "picked up again or finished", |jobs| {
        jobs.iter().all(|j| status_of(j) == "success")
            || jobs.iter().any(|j| !common::is_terminal(&status_of(j)))
    })
    .await?;
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            repo,
            &[
                "wait",
                "--run-index",
                &run.to_string(),
                "--json",
                "--timeout",
                "3m",
            ],
        ))
        .timeout(Duration::from_secs(200))
        .json()
        .await?;
    ensure!(v["status"] == "success", "rerun did not succeed: {v}");
    Ok(())
}
