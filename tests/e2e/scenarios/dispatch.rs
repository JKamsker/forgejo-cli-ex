//! `actions trigger` (dry-run, plain, `--wait` success and failure), with the
//! dispatched job reading the secret/variable set by the settings scenario.

use eyre::{ensure, Result};

use super::common;
use super::settings;
use super::Ctx;

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("dispatch-inputs");
    let greeting = format!("hi-{}", ctx.suffix);
    let greeting_input = format!("greeting={greeting}");

    ctx.step("trigger --dry-run --json");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "dispatch.yml",
                "--input",
                &greeting_input,
                "--dry-run",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(v["dryRun"] == true, "dry run: {v}");
    ensure!(
        v["url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/actions/workflows/dispatch.yml/dispatches")),
        "dry run url: {v}"
    );
    ensure!(
        v["body"]["ref"] == "refs/heads/main"
            && v["body"]["inputs"]["greeting"] == greeting.as_str(),
        "body: {v}"
    );

    ctx.step("trigger rejects malformed inputs");
    ctx.fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "dispatch.yml",
                "--input",
                "no-equals-sign",
            ],
        ))
        .fails()
        .await?;

    ctx.step("trigger (no wait) creates a run that sees inputs, secret and variable");
    let before = common::latest_run_index(ctx, &repo, "dispatch.yml").await?;
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "dispatch.yml",
                "--ref",
                "main",
                "--input",
                &greeting_input,
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(
        v["triggered"] == true && v["dryRun"] == false,
        "trigger: {v}"
    );
    let run = common::wait_new_run(ctx, &repo, "dispatch.yml", before).await?;
    let status = common::wait_run_done(ctx, &repo, run).await?;
    ensure!(status == "success", "dispatched run {run} ended {status}");
    let log = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "logs",
                "job",
                "--run-index",
                &run.to_string(),
                "--job-index",
                "0",
            ],
        ))
        .ok()
        .await?
        .stdout;
    ensure!(
        log.contains(&format!("fj-ex-e2e: greeting={greeting}")),
        "greeting input missing:\n{log}"
    );
    let secret = settings::secret_value(ctx);
    ensure!(
        log.contains(&format!("fj-ex-e2e: secret-length={}", secret.len())),
        "secret not delivered to the job (expected length {}):\n{log}",
        secret.len()
    );
    ensure!(
        !log.contains(&secret),
        "the secret value leaked into the job log"
    );
    ensure!(
        log.contains(&format!(
            "fj-ex-e2e: variable={}",
            settings::variable_value(ctx)
        )),
        "variable not delivered to the job:\n{log}"
    );

    ctx.step("trigger --wait --json (success) exits 0");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "dispatch.yml",
                "--ref",
                "refs/heads/main",
                "--input",
                "outcome=success",
                "--wait",
                "--interval",
                "1",
                "--timeout",
                "4m",
                "--json",
            ],
        ))
        .timeout(std::time::Duration::from_secs(300))
        .json()
        .await?;
    ensure!(
        v["status"] == "success" && v["trigger"]["triggered"] == true,
        "trigger --wait: {v}"
    );
    ensure!(
        v["runIndex"].as_i64() > Some(run),
        "trigger --wait waited for an old run: {v}"
    );

    ctx.step("trigger --wait (failure) exits non-zero");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &[
                "trigger",
                "--workflow",
                "dispatch.yml",
                "--input",
                "outcome=failure",
                "--wait",
                "--interval",
                "1",
                "--timeout",
                "4m",
            ],
        ))
        .timeout(std::time::Duration::from_secs(300))
        .fails()
        .await?;
    ensure!(
        out.stdout.contains("Triggered workflow 'dispatch.yml'"),
        "stdout:\n{}",
        out.stdout
    );
    ensure!(
        out.stdout.contains("completed with status 'failure'"),
        "stdout:\n{}",
        out.stdout
    );
    ensure!(out.stderr.contains("failure"), "stderr:\n{}", out.stderr);
    Ok(())
}
