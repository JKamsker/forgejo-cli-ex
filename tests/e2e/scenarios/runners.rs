//! `actions runners token` (all scopes) and `actions runners jobs`.

use std::time::Duration;

use eyre::{ensure, Result};
use serde_json::Value;

use super::common;
use super::Ctx;
use crate::e2e::cli::redact_json;
use crate::e2e::harness::util;

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    let repo = ctx.repo("ci-artifacts");
    let org = ctx.stack.org.clone();

    for (scope, extra) in [
        ("repo", vec![]),
        ("global", vec!["--scope", "global"]),
        ("user", vec!["--scope", "user"]),
        ("org", vec!["--scope", "org", "--org", org.as_str()]),
    ] {
        ctx.step(&format!("runners token ({scope} scope)"));
        let mut args = vec!["runners", "token", "--json"];
        args.extend(extra);
        let v = ctx.fj.cmd(&ctx.actions(&repo, &args)).json().await?;
        // Assertion messages show the document with the registration token masked.
        let shown = redact_json(&v);
        ensure!(
            v["baseUrl"] == base && v["scope"] == scope,
            "{scope} token: {shown}"
        );
        ensure!(
            v["token"].as_str().is_some_and(|t| !t.trim().is_empty()),
            "{scope} token empty: {shown}"
        );
        if scope == "repo" {
            ensure!(v["repo"] == repo.as_str(), "repo token repo: {shown}");
        }
        if scope == "org" {
            ensure!(v["org"] == org.as_str(), "org token org: {shown}");
        }
    }

    ctx.step("runners token (plain text)");
    let out = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["runners", "token"]))
        .ok()
        .await?;
    let lines = out.stdout.trim().lines().count();
    // The output is the token itself: report the line count, not the text.
    ensure!(
        lines == 1,
        "expected only the token on one line, got {lines} lines"
    );

    if !ctx.caps().runner_jobs_api {
        ctx.skip_capability("runners jobs (empty queue)", "runner_jobs_api");
        return Ok(());
    }

    // GitHub issue #13: Forgejo answers `null` (not `[]`) for an empty queue.
    let empty_repo = ctx.repo("pulls-demo");
    ctx.step("runners jobs --json on a repository without jobs is an empty list");
    let v = ctx
        .fj
        .cmd(&ctx.actions(&empty_repo, &["runners", "jobs", "--json"]))
        .json()
        .await?;
    ensure!(
        v["jobs"] == Value::Array(vec![]),
        "expected jobs: [], got {v}"
    );
    ensure!(
        v["scope"] == "repo" && v["repo"] == empty_repo.as_str(),
        "runner jobs: {v}"
    );
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &empty_repo,
            &["runners", "jobs", "--waiting", "--label", "nope", "--json"],
        ))
        .json()
        .await?;
    ensure!(
        v["jobs"] == Value::Array(vec![]) && v["waitingOnly"] == true,
        "filtered empty: {v}"
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(&empty_repo, &["runners", "jobs", "--header"]))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("RunsOn"),
        "expected a header row:\n{}",
        out.stdout
    );

    ctx.step("runners jobs --scope global/user --json");
    for scope in ["global", "user"] {
        let v = ctx
            .fj
            .cmd(&[
                "actions", "--host", base, "runners", "jobs", "--scope", scope, "--json",
            ])
            .json()
            .await?;
        ensure!(v["jobs"].is_array(), "{scope} jobs: {v}");
    }
    Ok(())
}

/// A job whose `runs-on` no runner serves shows up as waiting; then cancel it.
pub async fn waiting_queue(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("dispatch-inputs");
    let label = "fjex-no-such-runner";
    let before = common::latest_run_index(ctx, &repo, "waiting.yml").await?;

    ctx.step("dispatch waiting.yml (no runner has its label)");
    ctx.fj
        .cmd(&ctx.actions(
            &repo,
            &["trigger", "--workflow", "waiting.yml", "--ref", "main"],
        ))
        .ok()
        .await?;
    let run = common::wait_new_run(ctx, &repo, "waiting.yml", before).await?;

    if ctx.caps().runner_jobs_api {
        ctx.step("runners jobs --waiting --label lists the queued job");
        let jobs = util::poll(
            "a waiting runner job",
            Duration::from_secs(90),
            Duration::from_secs(2),
            || {
                let args = ctx.actions(
                    &repo,
                    &["runners", "jobs", "--waiting", "--label", label, "--json"],
                );
                async move {
                    let v = ctx.fj.cmd(&args).json().await?;
                    Ok(v["jobs"]
                        .as_array()
                        .filter(|a| !a.is_empty())
                        .map(|_| v.clone()))
                }
            },
        )
        .await?;
        ensure!(
            jobs["waitingOnly"] == true && jobs["labels"][0] == label,
            "runner jobs: {jobs}"
        );
        let job = &jobs["jobs"][0];
        ensure!(job["name"] == "queued", "queued job name: {jobs}");
        ensure!(
            job["runs_on"]
                .as_array()
                .is_some_and(|a| a.iter().any(|l| l == label)),
            "runs_on: {jobs}"
        );
        let out = ctx
            .fj
            .cmd(&ctx.actions(&repo, &["runners", "jobs", "--waiting", "--no-header"]))
            .ok()
            .await?;
        ensure!(
            out.stdout.contains("queued") && out.stdout.contains(label),
            "table:\n{}",
            out.stdout
        );
    } else {
        ctx.skip_capability(
            "runners jobs --waiting (queued job listing)",
            "runner_jobs_api",
        );
    }

    ctx.step("the waiting run is visible as waiting, then canceled");
    let status = common::run_status(ctx, &repo, run)
        .await?
        .unwrap_or_default();
    ensure!(
        status == "waiting" || status == "blocked",
        "waiting run status: {status}"
    );
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["cancel", "--run-index", &run.to_string(), "--json"],
        ))
        .json()
        .await?;
    ensure!(v["canceled"] == true, "cancel: {v}");
    common::wait_run_status(ctx, &repo, run, "canceled", Duration::from_secs(60), |s| {
        s == "canceled"
    })
    .await?;
    Ok(())
}
