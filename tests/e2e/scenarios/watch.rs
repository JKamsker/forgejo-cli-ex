//! `actions logs job --follow` and `actions jobs --watch --json-lines`,
//! run concurrently against one live slow job.

use std::time::Duration;

use eyre::{ensure, eyre, Result};
use serde_json::Value;

use super::common;
use super::Ctx;

const TICKS: usize = 12;

pub async fn follow_and_watch(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("slow-job");
    let before = common::latest_run_index(ctx, &repo, "slow.yml").await?;
    ctx.step(&format!("dispatch slow.yml with {TICKS} ticks"));
    ctx.fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "slow.yml",
                "--input",
                &format!("seconds={TICKS}"),
            ],
        ))
        .ok()
        .await?;
    let run = common::wait_new_run(ctx, &repo, "slow.yml", before).await?;
    let run_s = run.to_string();

    ctx.step("follow the log and watch the jobs at the same time");
    let watch_args = ctx.actions(
        &repo,
        &[
            "jobs",
            "--run-index",
            &run_s,
            "--watch",
            "--watch-interval",
            "1",
            "--json-lines",
        ],
    );
    let follow_args = ctx.actions(
        &repo,
        &[
            "logs",
            "job",
            "--run-index",
            &run_s,
            "--job-index",
            "0",
            "--follow",
            "--follow-interval",
            "1",
        ],
    );
    let limit = Duration::from_secs(240);
    let (watch, follow) = tokio::join!(
        ctx.fj.cmd(&watch_args).timeout(limit).output(),
        ctx.fj.cmd(&follow_args).timeout(limit).output()
    );
    let (watch, follow) = (watch?, follow?);
    ensure!(
        watch.success(),
        "jobs --watch failed:\n{}\n{}",
        watch.stdout,
        watch.stderr
    );
    ensure!(
        follow.success(),
        "logs --follow failed:\n{}\n{}",
        follow.stdout,
        follow.stderr
    );

    ctx.step("--json-lines emits one JSON document per line, ending complete");
    let mut snapshots = Vec::new();
    for (n, line) in watch
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
    {
        let v: Value = serde_json::from_str(line).map_err(|e| {
            eyre!(
                "--json-lines line {n} is not a JSON document ({e}): {line:?}\nfull stdout:\n{}",
                watch.stdout
            )
        })?;
        snapshots.push(v);
    }
    ensure!(
        snapshots.len() >= 2,
        "expected initial + terminal snapshots, got {}",
        snapshots.len()
    );
    let last = snapshots.last().unwrap();
    ensure!(
        last["complete"] == true && last["runIndex"] == run,
        "last snapshot: {last}"
    );
    ensure!(
        common::status_of(&last["jobs"][0]) == "success",
        "last snapshot job: {last}"
    );
    ensure!(
        snapshots.iter().any(|s| s["complete"] == false),
        "no in-progress snapshot was observed"
    );
    for pair in snapshots.windows(2) {
        let sig = |v: &Value| v["jobs"].to_string() + &v["complete"].to_string();
        ensure!(
            sig(&pair[0]) != sig(&pair[1]) || pair[1]["complete"] == true,
            "duplicate snapshot emitted"
        );
    }

    ctx.step("--follow printed every tick exactly once, in order");
    let mut last_seen = 0;
    for i in 1..=TICKS {
        let needle = format!("fj-ex-e2e: tick {i}/{TICKS}");
        let count = follow.stdout.matches(&needle).count();
        ensure!(
            count == 1,
            "{needle:?} printed {count} times:\n{}",
            follow.stdout
        );
        let pos = follow.stdout.find(&needle).unwrap();
        ensure!(pos >= last_seen, "tick {i} out of order");
        last_seen = pos;
    }
    ensure!(
        follow.stdout.contains("fj-ex-e2e: slow job done"),
        "follow missed the tail:\n{}",
        follow.stdout
    );

    ctx.step("logs job after completion matches the followed output");
    let full = ctx
        .fj
        .cmd(&follow_args[..follow_args.len() - 3])
        .ok()
        .await?;
    ensure!(
        full.stdout.contains("fj-ex-e2e: slow job done"),
        "full log:\n{}",
        full.stdout
    );
    Ok(())
}
