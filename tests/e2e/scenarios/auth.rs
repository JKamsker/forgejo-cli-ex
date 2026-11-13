//! `auth login/status/list/show/clear-cookies/logout`, 2FA login, and the
//! missing-API-token guidance.

use eyre::{ensure, Result};
use serde_json::Value;

use super::Ctx;
use crate::e2e::cli::FjEx;
use crate::e2e::harness::stack::write_fj_keys_json;
use crate::e2e::harness::{totp, ui};

pub async fn login(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    let admin = &ctx.stack.admin;

    ctx.step("runners token without an fj API token explains how to authenticate");
    let out = ctx
        .fj
        .cmd(&[
            "actions", "--host", base, "runners", "token", "--scope", "global",
        ])
        .fails()
        .await?;
    for needle in ["No Forgejo API token found", "fj --host", "keys.json"] {
        ensure!(
            out.stderr.contains(needle),
            "expected {needle:?} in stderr:\n{}",
            out.stderr
        );
    }

    ctx.step("auth login --password-stdin");
    let out = ctx
        .fj
        .cmd(&[
            "auth",
            "login",
            "--host",
            base,
            "--username",
            &admin.username,
            "--password-stdin",
        ])
        .stdin(format!("{}\n", admin.password))
        .ok()
        .await?;
    ensure!(
        !out.stdout.contains(&admin.password) && !out.stderr.contains(&admin.password),
        "login output echoed the password"
    );

    ctx.step("auth status --json");
    let status = ctx
        .fj
        .cmd(&["auth", "status", "--host", base, "--json"])
        .json()
        .await?;
    ensure!(status["baseUrl"] == base, "status baseUrl: {status}");
    ensure!(
        status["username"] == admin.username.as_str(),
        "status username: {status}"
    );
    ensure!(
        status["hasCreds"] == true && status["sessionOk"] == true,
        "status: {status}"
    );

    ctx.step("auth list --json / auth show --json");
    let list = ctx.fj.cmd(&["auth", "list", "--json"]).json().await?;
    ensure!(has_login(&list, base), "auth list misses {base}: {list}");
    let show = ctx
        .fj
        .cmd(&["auth", "show", "--host", base, "--json"])
        .json()
        .await?;
    ensure!(
        show["username"] == admin.username.as_str() && show["hasPassword"] == true,
        "show: {show}"
    );
    ensure!(
        !show.to_string().contains(&admin.password),
        "auth show leaked the password: {show}"
    );

    ctx.step("auth clear-cookies, then status re-logs in from stored creds");
    ctx.fj
        .cmd(&["auth", "clear-cookies", "--host", base])
        .ok()
        .await?;
    let status = ctx
        .fj
        .cmd(&["auth", "status", "--host", base, "--json"])
        .json()
        .await?;
    ensure!(
        status["sessionOk"] == true,
        "status after clear-cookies: {status}"
    );

    // Stand-in for `fj auth login`: the API token store fj-ex reads.
    write_fj_keys_json(ctx.fj.appdata(), base, &admin.token)?;

    ctx.step("reviewer logs in (UI creds only, no fj API token)");
    let reviewer = &ctx.stack.reviewer;
    ctx.reviewer
        .cmd(&[
            "auth",
            "login",
            "--host",
            base,
            "--username",
            &reviewer.username,
            "--password-stdin",
        ])
        .stdin(format!("{}\n", reviewer.password))
        .ok()
        .await?;
    Ok(())
}

pub async fn two_factor(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    let username = "e2e-otp";
    let password = "e2e-otp-password-1234";
    ctx.stack
        .api
        .create_user(
            &ctx.stack.admin.token,
            username,
            password,
            "e2e-otp@example.invalid",
        )
        .await?;

    ctx.step("enroll TOTP through the web UI");
    let web = ui::WebSession::login(base, username, password).await?;
    let secret = web.enroll_totp().await?;

    let fj = FjEx::new(
        &ctx.stack.work_dir.join("fj-ex").join("appdata-otp"),
        ctx.tag(),
    )?;

    ctx.step("login without a passcode fails");
    let out = fj
        .cmd(&[
            "auth",
            "login",
            "--host",
            base,
            "--username",
            username,
            "--password-stdin",
        ])
        .stdin(format!("{password}\n"))
        .fails()
        .await?;
    for hint in ["two-factor passcode", "--otp-stdin", "--otp", "FJ_OTP"] {
        ensure!(
            out.stderr.contains(hint),
            "no-TTY 2FA error lacks {hint:?}:\n{}",
            out.stderr
        );
    }
    let status = fj
        .cmd(&["auth", "status", "--host", base, "--json", "--no-relogin"])
        .output()
        .await?;
    ensure!(
        !status.success() || status.json().is_ok_and(|v| v["sessionOk"] != true),
        "a login without passcode produced a session: {}",
        status.stdout
    );

    ctx.step("login with --password-stdin --otp-stdin");
    // The enrollment consumed the current window's code; the next window's
    // code is accepted thanks to the servers' one-step skew.
    let code = totp::totp_now(&secret, 1)?;
    fj.cmd(&[
        "auth",
        "login",
        "--host",
        base,
        "--username",
        username,
        "--password-stdin",
        "--otp-stdin",
    ])
    .stdin(format!("{password}\n{code}\n"))
    .ok()
    .await?;
    let status = fj
        .cmd(&["auth", "status", "--host", base, "--json", "--no-relogin"])
        .json()
        .await?;
    ensure!(
        status["username"] == username && status["sessionOk"] == true,
        "2FA status: {status}"
    );

    ctx.step("a 2FA session can read Actions data");
    let repo = ctx.repo("ci-artifacts");
    fj.cmd(&[
        "actions",
        "--host",
        base,
        "--repo",
        &repo,
        "workflows",
        "--json",
    ])
    .json()
    .await?;
    Ok(())
}

pub async fn logout(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    ctx.fj.cmd(&["auth", "logout", "--host", base]).ok().await?;
    let list = ctx.fj.cmd(&["auth", "list", "--json"]).json().await?;
    ensure!(
        !has_login(&list, base),
        "logout left {base} in auth list: {list}"
    );
    // The UI backend needs the removed login ...
    let repo = ctx.repo("ci-artifacts");
    let out = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["--backend", "ui", "runs", "--latest"]))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("auth login"),
        "expected a login hint after logout:\n{}",
        out.stderr
    );
    // ... while `auto` falls back to the API token left in fj's keys.json.
    let v = ctx
        .fj
        .cmd(&ctx.actions(&repo, &["runs", "--latest", "--json"]))
        .json()
        .await?;
    ensure!(
        v["runs"].as_array().is_some_and(|r| r.len() == 1),
        "token fallback after logout: {v}"
    );
    Ok(())
}

fn has_login(list: &Value, base: &str) -> bool {
    list["logins"]
        .as_array()
        .is_some_and(|a| a.iter().any(|l| l["baseUrl"] == base))
}
