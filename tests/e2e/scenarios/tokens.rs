//! `token mint nuget` and `token list`.

use eyre::{ensure, eyre, Result};

use super::Ctx;
use crate::e2e::cli::redact_json;
use crate::e2e::harness::api::Auth;

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    let user = &ctx.stack.admin.username;

    ctx.step("token mint nuget --json");
    let minted = ctx
        .fj
        .cmd(&["token", "mint", "nuget", "--host", base, "--json"])
        .json()
        .await?;
    // Assertion messages show the document with the API key masked.
    let shown = redact_json(&minted);
    ensure!(
        minted["baseUrl"] == base && minted["owner"] == user.as_str(),
        "mint: {shown}"
    );
    ensure!(
        minted["username"] == user.as_str(),
        "mint username: {shown}"
    );
    ensure!(
        minted["registryUrl"] == format!("{base}/api/packages/{user}/nuget/index.json").as_str(),
        "registry url: {shown}"
    );
    ensure!(minted["scope"] == "write:package", "scope: {shown}");
    let api_key = minted["apiKey"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("no apiKey: {shown}"))?;
    let write_name = minted["tokenName"]
        .as_str()
        .ok_or_else(|| eyre!("no tokenName"))?
        .to_string();

    ctx.step("the minted key authenticates against the API");
    let me = ctx.stack.api.get("/user", Auth::Token(api_key)).await;
    // A package-only token may not read /user on every server; it must at least not be rejected as invalid.
    if let Err(e) = &me {
        let msg = format!("{e:?}");
        ensure!(!msg.contains("401"), "minted token rejected: {msg}");
    }

    ctx.step("token mint nuget --read-only --owner <org> --token-name (text output)");
    let ro_name = format!("fjex-e2e-ro-{}", ctx.suffix);
    let out = ctx
        .fj
        .cmd(&[
            "token",
            "mint",
            "nuget",
            "--host",
            base,
            "--read-only",
            "--owner",
            &ctx.stack.org,
            "--token-name",
            &ro_name,
        ])
        .ok()
        .await?;
    ensure!(
        out.stdout
            .contains(&format!("FORGEJO_NUGET_USERNAME={user}")),
        "text output:\n{}",
        out.stdout
    );
    ensure!(
        out.stdout.contains(&format!(
            "FORGEJO_NUGET_SOURCE={base}/api/packages/{}/nuget/index.json",
            ctx.stack.org
        )),
        "text output:\n{}",
        out.stdout
    );
    ensure!(
        out.stdout.contains("FORGEJO_NUGET_API_KEY="),
        "text output:\n{}",
        out.stdout
    );

    ctx.step("token list --json shows both tokens with their scopes");
    let list = ctx
        .fj
        .cmd(&["token", "list", "--host", base, "--json", "--limit", "50"])
        .json()
        .await?;
    ensure!(
        list["baseUrl"] == base && list["username"] == user.as_str(),
        "token list: {list}"
    );
    let has = |name: &str, scope: &str| {
        list["tokens"].as_array().is_some_and(|ts| {
            ts.iter().any(|t| {
                t["name"] == name
                    && t["scopes"]
                        .as_array()
                        .is_some_and(|s| s.iter().any(|x| x == scope))
            })
        })
    };
    ensure!(
        has(&write_name, "write:package"),
        "write token missing from list: {list}"
    );
    ensure!(
        has(&ro_name, "read:package"),
        "read-only token missing from list: {list}"
    );
    ensure!(
        !list.to_string().contains(api_key),
        "token list leaked a token secret"
    );

    ctx.step("token list (table)");
    let out = ctx
        .fj
        .cmd(&["token", "list", "--host", base, "--header"])
        .ok()
        .await?;
    ensure!(
        out.stdout.contains(&ro_name) && out.stdout.contains("Scopes"),
        "table:\n{}",
        out.stdout
    );
    Ok(())
}
