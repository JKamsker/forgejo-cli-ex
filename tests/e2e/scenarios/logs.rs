//! `actions logs job` and `actions logs run` on finished runs.

use eyre::{ensure, eyre, Result};

use super::common;
use super::Ctx;

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let ci = ctx.repo("ci-artifacts");
    let mixed = ctx.repo("ci-mixed");
    let ci_run = common::latest_run_index(ctx, &ci, "ci.yml")
        .await?
        .ok_or_else(|| eyre!("no ci run"))?;
    let mixed_run = common::latest_run_index(ctx, &mixed, "mixed.yml")
        .await?
        .ok_or_else(|| eyre!("no mixed run"))?;
    let marker = format!("fj-ex-e2e: hello from {ci}");

    ctx.step("logs job (stdout)");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "logs",
                "job",
                "--run-index",
                &ci_run.to_string(),
                "--job-index",
                "0",
            ],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains(&marker),
        "job log lacks {marker:?}:\n{}",
        out.stdout
    );
    ensure!(
        out.stdout.contains("fj-ex-e2e: uploaded my-artifact"),
        "job log lacks upload line"
    );

    ctx.step("logs job --latest --workflow --attempt 1 --out-file");
    let file = ctx.scratch.join("logs").join("ci-job0.log");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "logs",
                "job",
                "--latest",
                "--workflow",
                "ci.yml",
                "--job-index",
                "0",
                "--attempt",
                "1",
                "--out-file",
                &file.to_string_lossy(),
            ],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.trim() == file.to_string_lossy(),
        "logs --out-file prints the path:\n{}",
        out.stdout
    );
    let text = std::fs::read_to_string(&file)?;
    ensure!(text.contains(&marker), "log file lacks marker");

    ctx.step("logs run --out-dir writes one file per job (incl. the failed job)");
    let dir = ctx.scratch.join("logs").join("mixed-run");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "logs",
                "run",
                "--run-index",
                &mixed_run.to_string(),
                "--out-dir",
                &dir.to_string_lossy(),
            ],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.matches("Saved: job").count() == 2,
        "logs run output:\n{}",
        out.stdout
    );
    let mut contents = String::new();
    for entry in std::fs::read_dir(&dir)? {
        contents.push_str(&std::fs::read_to_string(entry?.path())?);
    }
    ensure!(
        contents.contains("fj-ex-e2e: pass job ok"),
        "pass job log missing"
    );
    ensure!(
        contents.contains("fj-ex-e2e: failing on purpose"),
        "fail job log missing"
    );

    ctx.step("logs run (stdout, separators on stderr) and --max-jobs");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &["logs", "run", "--run-index", &mixed_run.to_string()],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("fj-ex-e2e: pass job ok") && out.stdout.contains("failing on purpose"),
        "stdout"
    );
    ensure!(
        out.stderr.contains("== job 0") && out.stderr.contains("== job 1"),
        "stderr:\n{}",
        out.stderr
    );
    ensure!(
        !out.stdout.contains("== job 0"),
        "separators must not be on stdout"
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "logs",
                "run",
                "--run-index",
                &mixed_run.to_string(),
                "--max-jobs",
                "1",
            ],
        ))
        .ok()
        .await?;
    ensure!(
        !out.stderr.contains("== job 1"),
        "--max-jobs 1 printed job 1:\n{}",
        out.stderr
    );
    Ok(())
}
