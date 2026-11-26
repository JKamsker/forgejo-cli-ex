//! `actions prioritize` and `actions deprioritize` on queued runs, and the
//! order in which a runner then picks them.

use std::time::Duration;

use eyre::{ensure, eyre, Result};

use super::common;
use super::Ctx;
use crate::e2e::harness::api::Auth;
use crate::e2e::harness::{docker, ui};

/// A job no runner can take.
const WAITING: &str = "waiting.yml";
/// A job only the runner of [`runner_picks_prioritized_first`] can take.
const PRIORITY: &str = "priority.yml";
/// `runs-on` of [`PRIORITY`].
const PRIORITY_LABEL: &str = "fjex-priority";

/// Dispatch a workflow whose label no running runner has and return the queued run.
async fn queue_run(ctx: &Ctx<'_>, repo: &str, workflow: &str) -> Result<i64> {
    let before = common::latest_run_index(ctx, repo, workflow).await?;
    ctx.fj
        .cmd(&ctx.actions(repo, &["trigger", "--workflow", workflow, "--ref", "main"]))
        .ok()
        .await?;
    common::wait_new_run(ctx, repo, workflow, before).await
}

async fn cancel_run(ctx: &Ctx<'_>, repo: &str, run: i64) -> Result<()> {
    ctx.fj
        .cmd(&ctx.actions(repo, &["cancel", "--run-index", &run.to_string()]))
        .ok()
        .await?;
    common::wait_run_status(ctx, repo, run, "canceled", Duration::from_secs(60), |s| {
        s == "canceled"
    })
    .await?;
    Ok(())
}

/// Whether the run list offers only "do not prioritize" for the run, which is
/// how the page shows a prioritized run whatever the locale.
async fn listed_as_prioritized(web: &ui::WebSession, repo: &str, run: i64) -> Result<bool> {
    let page = web.get_text(&format!("/{repo}/actions")).await?;
    let disabled = |action: &str| page.contains(&format!("/runs/{run}/{action}\" disabled"));
    let offered = |action: &str| page.contains(&format!("/runs/{run}/{action}\""));
    ensure!(
        offered("prioritize") && offered("deprioritize"),
        "run {run} has no prioritize buttons in the run list of {repo}"
    );
    ensure!(
        disabled("prioritize") != disabled("deprioritize"),
        "run {run}: exactly one of the two buttons must be disabled"
    );
    Ok(disabled("prioritize"))
}

/// Runs in the order a runner picked their jobs. Every pick creates a task,
/// and task ids only grow, so the order does not depend on when we look.
async fn pick_order(ctx: &Ctx<'_>, repo: &str, runs: &[i64]) -> Result<Vec<i64>> {
    let v = ctx
        .stack
        .api
        .get(
            &format!("/repos/{repo}/actions/tasks?limit=50"),
            Auth::Token(&ctx.stack.admin.token),
        )
        .await?;
    let mut picked: Vec<(i64, i64)> = v["workflow_runs"]
        .as_array()
        .ok_or_else(|| eyre!("no task list: {v}"))?
        .iter()
        .filter_map(|t| Some((t["id"].as_i64()?, t["run_number"].as_i64()?)))
        .filter(|(_, run)| runs.contains(run))
        .collect();
    picked.sort_unstable();
    ensure!(
        picked.len() == runs.len(),
        "expected one task for each of the runs {runs:?}: {v}"
    );
    Ok(picked.into_iter().map(|(_, run)| run).collect())
}

/// Queue three runs while no runner can take them, prioritize the newest,
/// then start a runner that takes one job at a time: it must pick the
/// prioritized run first and the other two in the order they were queued.
async fn runner_picks_prioritized_first(
    ctx: &Ctx<'_>,
    web: &ui::WebSession,
    repo: &str,
) -> Result<()> {
    ctx.step("queue three priority.yml runs and prioritize the newest");
    let mut runs = Vec::new();
    for _ in 0..3 {
        runs.push(queue_run(ctx, repo, PRIORITY).await?);
    }
    let (oldest, middle, newest) = (runs[0], runs[1], runs[2]);
    ctx.fj
        .cmd(&ctx.actions(repo, &["prioritize", "--run-index", &newest.to_string()]))
        .ok()
        .await?;
    ensure!(
        listed_as_prioritized(web, repo, newest).await?,
        "run {newest} is not prioritized"
    );
    for run in &runs {
        let status = common::run_status(ctx, repo, *run).await?;
        ensure!(
            status.as_deref() == Some("waiting"),
            "run {run} must still be queued before the runner starts: {status:?}"
        );
    }

    ctx.step("start a capacity-1 runner and let it drain the queue");
    let runner = ctx
        .stack
        .start_extra_runner("priority", PRIORITY_LABEL, 1)
        .await?;
    for run in &runs {
        let status = common::wait_run_done(ctx, repo, *run).await?;
        ensure!(status == "success", "run {run} ended as {status}");
    }
    // The runner has nothing left to do, and no other scenario expects it.
    docker::run(&["rm", "-f", "-v", &runner])?;

    let order = pick_order(ctx, repo, &runs).await?;
    ensure!(
        order == [newest, oldest, middle],
        "the runner picked the runs in the order {order:?}; expected the prioritized run \
         {newest} first, then {oldest} and {middle} as queued"
    );
    ctx.note(&format!(
        "a capacity-1 runner picked the prioritized run {newest} first, then {oldest} and {middle}"
    ));
    Ok(())
}

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("dispatch-inputs");
    ctx.step("queue two waiting.yml runs");
    let first = queue_run(ctx, &repo, WAITING).await?;
    let second = queue_run(ctx, &repo, WAITING).await?;
    let second_s = second.to_string();

    ctx.step("prioritize --dry-run --json");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "prioritize",
                "--run-index",
                &second_s,
                "--dry-run",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(
        v["dryRun"] == true
            && v["url"]
                .as_str()
                .is_some_and(|u| u.ends_with(&format!("/actions/runs/{second}/prioritize"))),
        "{v}"
    );

    ctx.step("the REST API backend refuses, naming the missing endpoint");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["--backend", "api", "prioritize", "--run-index", &second_s],
        ))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("no endpoint for prioritizing"),
        "api backend stderr:\n{}",
        out.stderr
    );

    if !ctx.caps().ui_run_priority {
        ctx.step("prioritize fails, saying the server has no run prioritization");
        let out = ctx
            .fj
            .cmd(&ctx.actions(&repo, &["prioritize", "--run-index", &second_s]))
            .fails()
            .await?;
        ensure!(
            out.stderr.contains("no run prioritization"),
            "prioritize stderr:\n{}",
            out.stderr
        );
        ctx.skip_capability("actions prioritize / deprioritize", "ui_run_priority");
        cancel_run(ctx, &repo, first).await?;
        cancel_run(ctx, &repo, second).await?;
        return Ok(());
    }

    let admin = &ctx.stack.admin;
    let web = ui::WebSession::login(ctx.base(), &admin.username, &admin.password).await?;
    ensure!(
        !listed_as_prioritized(&web, &repo, second).await?,
        "the dry run prioritized run {second}"
    );

    ctx.step("prioritize --run-index");
    let out = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["prioritize", "--run-index", &second_s]))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains(&format!("Prioritized run #{second}")),
        "prioritize output:\n{}",
        out.stdout
    );
    ensure!(
        listed_as_prioritized(&web, &repo, second).await?,
        "run {second} is not prioritized"
    );
    ensure!(
        !listed_as_prioritized(&web, &repo, first).await?,
        "run {first} must stay unprioritized"
    );

    ctx.step("deprioritize --latest --workflow --json");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["deprioritize", "--latest", "--workflow", WAITING, "--json"],
        ))
        .json()
        .await?;
    ensure!(
        v["prioritized"] == false && v["runIndex"] == second,
        "deprioritize: {v}"
    );
    ensure!(
        !listed_as_prioritized(&web, &repo, second).await?,
        "run {second} is still prioritized"
    );

    ctx.step("a done run and an unknown run are refused");
    cancel_run(ctx, &repo, first).await?;
    let out = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["prioritize", "--run-index", &first.to_string()]))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("already done"),
        "done run stderr:\n{}",
        out.stderr
    );
    let missing = (second + 1000).to_string();
    let out = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["prioritize", "--run-index", &missing]))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("was not found"),
        "unknown run stderr:\n{}",
        out.stderr
    );
    cancel_run(ctx, &repo, second).await?;

    runner_picks_prioritized_first(ctx, &web, &repo).await
}
