//! `actions artifacts list` / `actions artifacts get`.

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

    ctx.step("artifacts list --json");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "artifacts",
                "list",
                "--run-index",
                &ci_run.to_string(),
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(
        v["runIndex"] == ci_run && v["artifactCount"] == 1,
        "artifacts: {v}"
    );
    let artifact = &v["artifacts"][0];
    ensure!(artifact["name"] == "my-artifact", "artifact: {v}");

    ctx.step("artifacts list (table, --latest --workflow)");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "artifacts",
                "list",
                "--latest",
                "--workflow",
                "ci.yml",
                "--header",
            ],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("my-artifact") && out.stdout.contains("Name"),
        "table:\n{}",
        out.stdout
    );

    ctx.step("artifacts get downloads a zip");
    let zip = ctx.scratch.join("artifacts").join("my-artifact.zip");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "artifacts",
                "get",
                "--run-index",
                &ci_run.to_string(),
                "--artifact",
                "my-artifact",
                "--out-file",
                &zip.to_string_lossy(),
            ],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.trim() == zip.to_string_lossy(),
        "get prints the path:\n{}",
        out.stdout
    );
    let bytes = std::fs::read(&zip)?;
    ensure!(bytes.starts_with(b"PK"), "not a zip: {:?}", bytes.get(..4));
    ensure!(
        bytes
            .windows(b"artifact.txt".len())
            .any(|w| w == b"artifact.txt"),
        "zip does not contain artifact.txt"
    );

    ctx.step("artifacts get for an unknown artifact fails");
    let missing = ctx.scratch.join("artifacts").join("missing.zip");
    ctx.fj
        .cmd(&ctx.actions(
            &ci,
            &[
                "artifacts",
                "get",
                "--run-index",
                &ci_run.to_string(),
                "--artifact",
                "no-such-artifact",
                "--out-file",
                &missing.to_string_lossy(),
            ],
        ))
        .fails()
        .await?;

    ctx.step("a run without uploads has no artifacts");
    let v = ctx
        .fj
        .cmd(&ctx.actions(
            &mixed,
            &[
                "artifacts",
                "list",
                "--run-index",
                &mixed_run.to_string(),
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(v["artifactCount"] == 0, "mixed artifacts: {v}");
    Ok(())
}
