//! `actions secrets set` / `actions variables set` (`--value-stdin`).
//! The values are consumed later by `dispatch.yml` (see the trigger scenario).

use eyre::{ensure, Result};

use super::Ctx;

pub const SECRET_NAME: &str = "FJEX_E2E_SECRET";
pub const VARIABLE_NAME: &str = "FJEX_E2E_VARIABLE";

/// Secret value for this run; its length is asserted in the job log.
pub fn secret_value(ctx: &Ctx<'_>) -> String {
    format!("fjex-secret-value-{}-do-not-print", ctx.suffix)
}

pub fn variable_value(ctx: &Ctx<'_>) -> String {
    format!("fjex-variable-{}", ctx.suffix)
}

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("dispatch-inputs");
    let secret = secret_value(ctx);

    ctx.step("secrets set --value-stdin (create, then replace)");
    for value in ["placeholder-value".to_string(), secret.clone()] {
        let out = ctx
            .fj
            .cmd(&ctx.actions(
                &repo,
                &["secrets", "set", "--name", SECRET_NAME, "--value-stdin"],
            ))
            .stdin(format!("{value}\n"))
            .ok()
            .await?;
        ensure!(
            out.stdout.contains(SECRET_NAME),
            "secrets set output:\n{}",
            out.stdout
        );
        ensure!(
            !out.stdout.contains(&value) && !out.stderr.contains(&value),
            "secrets set echoed the secret value"
        );
    }

    ctx.step("variables set --value-stdin (create, then replace)");
    for value in ["placeholder".to_string(), variable_value(ctx)] {
        ctx.fj
            .cmd(&ctx.actions(
                &repo,
                &["variables", "set", "--name", VARIABLE_NAME, "--value-stdin"],
            ))
            .stdin(value)
            .ok()
            .await?;
    }

    ctx.step("invalid names and empty values are rejected");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["secrets", "set", "--name", "1-invalid", "--value-stdin"],
        ))
        .stdin("x")
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("Invalid Actions secret name"),
        "stderr:\n{}",
        out.stderr
    );
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &repo,
            &["variables", "set", "--name", "EMPTY_VALUE", "--value-stdin"],
        ))
        .stdin("\n")
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("must not be empty"),
        "stderr:\n{}",
        out.stderr
    );
    Ok(())
}
