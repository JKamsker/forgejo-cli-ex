//! Token-only automation (REST backend), stored tokens, token precedence and
//! imported browser sessions.
//!
//! The REST API differs per server version, so every operation is checked
//! against the target's [`Api`] capability: supported operations must work,
//! missing ones must be refused with fj-ex's "added in <release>" or
//! "no endpoint" message (and the gate is logged).

use std::time::Duration;

use eyre::{ensure, eyre, Result};
use serde_json::Value;

use super::common::{self, status_of};
use super::Ctx;
use crate::e2e::cli::{FjEx, Output};
use crate::e2e::harness::targets::{Api, Family};
use crate::e2e::harness::{ui, util};

/// Scopes of the automation token (no admin/org/user write access).
const AUTOMATION_SCOPES: &[&str] = &["read:repository", "write:repository", "read:user"];

fn fresh_fj(ctx: &Ctx<'_>, name: &str) -> Result<FjEx> {
    FjEx::new(
        &ctx.stack
            .work_dir
            .join("fj-ex")
            .join(format!("appdata-{name}-{}", util::unique_suffix())),
        ctx.tag(),
    )
}

async fn automation_token(ctx: &Ctx<'_>, scopes: &[&str]) -> Result<String> {
    let admin = &ctx.stack.admin;
    ctx.stack
        .api
        .create_token(
            &admin.username,
            &admin.password,
            &format!("fjex-e2e-{}", util::unique_suffix()),
            scopes,
        )
        .await
}

/// Assert a token-only command against the target's REST capability.
/// Returns the output when the operation is supported.
async fn expect_api(
    ctx: &Ctx<'_>,
    fj: &FjEx,
    token: &str,
    capability: &str,
    support: Api,
    args: &[String],
) -> Result<Option<Output>> {
    let out = fj.cmd(args).env("FJ_EX_TOKEN", token).output().await?;
    ensure!(
        !out.stdout.contains(token) && !out.stderr.contains(token),
        "output leaked the API token"
    );
    match support {
        Api::Yes => {
            ensure!(
                out.success(),
                "{capability}: token-only command failed:\n{}\n{}",
                out.stdout,
                out.stderr
            );
            Ok(Some(out))
        }
        gated => {
            expect_refusal(ctx, capability, gated, &out)?;
            Ok(None)
        }
    }
}

/// Assert that fj-ex refused a token-only command the target's REST API
/// cannot serve, with the "added in <release>" / "no endpoint for" message.
fn expect_refusal(ctx: &Ctx<'_>, capability: &str, support: Api, out: &Output) -> Result<()> {
    ensure!(
        !out.success(),
        "{capability}: expected a refusal, got:\n{}",
        out.stdout
    );
    let needle = match support {
        Api::Yes => return Err(eyre!("{capability}: supported, no refusal expected")),
        Api::Since(release) => format!("added in {release}"),
        Api::No => "no endpoint for".to_string(),
    };
    ensure!(
        out.stderr.contains(&needle),
        "{capability}: expected {needle:?} in:\n{}",
        out.stderr
    );
    ctx.note(&match support {
        Api::Since(release) => format!(
            "GATED {capability}: {} has no REST endpoint (added in {release}); fj-ex refused as expected",
            ctx.stack.target.label()
        ),
        _ => format!(
            "GATED {capability}: {} REST API has no such endpoint; fj-ex refused as expected",
            ctx.stack.target.label()
        ),
    });
    Ok(())
}

/// Cancel a queued run (waiting.yml: no runner has its label) with only the
/// token, and check that the server really canceled it.
async fn token_cancel(ctx: &Ctx<'_>, fj: &FjEx, token: &str, repo: &str) -> Result<()> {
    let before = common::latest_run_index(ctx, repo, "waiting.yml").await?;
    ctx.fj
        .cmd(&ctx.actions(
            repo,
            &["trigger", "--workflow", "waiting.yml", "--ref", "main"],
        ))
        .ok()
        .await?;
    let run = common::wait_new_run(ctx, repo, "waiting.yml", before).await?;
    let out = fj
        .cmd(&ctx.actions(repo, &["cancel", "--run-index", &run.to_string(), "--json"]))
        .env("FJ_EX_TOKEN", token)
        .output()
        .await?;
    ensure!(
        !out.stdout.contains(token) && !out.stderr.contains(token),
        "output leaked the API token"
    );
    ensure!(
        out.success(),
        "token-only cancel failed:\n{}\n{}",
        out.stdout,
        out.stderr
    );
    let v = out.json()?;
    ensure!(v["canceled"] == true, "token cancel: {v}");
    common::wait_run_status(ctx, repo, run, "canceled", Duration::from_secs(60), |s| {
        s == "canceled"
    })
    .await?;
    Ok(())
}

fn run_url_number(url: &str) -> Option<i64> {
    let rest = url.split("/actions/runs/").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Checks 1-6, 13 and 14: `actions` with nothing but `FJ_EX_TOKEN`.
pub async fn token_only_actions(ctx: &Ctx<'_>) -> Result<()> {
    let caps = ctx.caps();
    let token = automation_token(ctx, AUTOMATION_SCOPES).await?;
    let fj = fresh_fj(ctx, "token-only")?;
    let ci = ctx.repo("ci-artifacts");
    let dispatch = ctx.repo("dispatch-inputs");

    ctx.step("runs --json with only FJ_EX_TOKEN; runIndex is the web URL number");
    for repo in [&ci, &dispatch] {
        let out = fj
            .cmd(&ctx.actions(repo, &["runs", "--json"]))
            .env("FJ_EX_TOKEN", &token)
            .ok()
            .await?;
        let api_runs = out.json()?["runs"].as_array().cloned().unwrap_or_default();
        ensure!(!api_runs.is_empty(), "token runs list for {repo} is empty");
        for run in &api_runs {
            let url = run["url"].as_str().unwrap_or_default();
            ensure!(
                run_url_number(url) == run["runIndex"].as_i64(),
                "runIndex differs from the web URL number: {run}"
            );
        }
        // The UI backend lists the same numbers (index on Forgejo/Gitea <= 1.25, id on Gitea >= 1.26).
        let mut ui: Vec<i64> = common::runs(ctx, repo, None)
            .await?
            .iter()
            .filter_map(|r| r["runIndex"].as_i64())
            .collect();
        let mut api: Vec<i64> = api_runs
            .iter()
            .filter_map(|r| r["runIndex"].as_i64())
            .collect();
        ui.sort_unstable();
        api.sort_unstable();
        ensure!(
            ui == api,
            "{repo}: UI run numbers {ui:?} != API run numbers {api:?}"
        );
    }
    let ci_run = common::latest_run_index(ctx, &ci, "ci.yml")
        .await?
        .ok_or_else(|| eyre!("no ci run"))?;
    let ci_s = ci_run.to_string();

    ctx.step("jobs with only a token");
    if let Some(out) = expect_api(
        ctx,
        &fj,
        &token,
        "api_jobs",
        caps.api_jobs,
        &ctx.actions(&ci, &["jobs", "--run-index", &ci_s, "--json"]),
    )
    .await?
    {
        let jobs = out.json()?["jobs"].as_array().cloned().unwrap_or_default();
        ensure!(
            jobs.iter()
                .any(|j| j["name"] == "build" && status_of(j) == "success"),
            "token jobs: {jobs:?}"
        );
    }

    ctx.step("logs job with only a token");
    if let Some(out) = expect_api(
        ctx,
        &fj,
        &token,
        "api_logs",
        caps.api_logs,
        &ctx.actions(
            &ci,
            &["logs", "job", "--run-index", &ci_s, "--job-index", "0"],
        ),
    )
    .await?
    {
        ensure!(
            out.stdout.contains(&format!("fj-ex-e2e: hello from {ci}")),
            "token job log lacks the step output:\n{}",
            out.stdout
        );
    }

    ctx.step("artifacts list with only a token");
    // The fixture uploads with the v3 protocol. Gitea's REST API only covers
    // v4 artifacts, so the list is empty there and fj-ex must say why.
    let api_artifacts = if ctx.stack.target.family == Family::Forgejo {
        caps.api_jobs
    } else {
        Api::Yes
    };
    if let Some(out) = expect_api(
        ctx,
        &fj,
        &token,
        "api_artifacts",
        api_artifacts,
        &ctx.actions(&ci, &["artifacts", "list", "--run-index", &ci_s, "--json"]),
    )
    .await?
    {
        let v = out.json()?;
        let count = v["artifactCount"].as_i64().unwrap_or(-1);
        if ctx.stack.target.family == Family::Gitea {
            ensure!(
                count == 0 && out.stderr.contains("v4 protocol"),
                "token artifacts on Gitea (v3 upload): {v}\n{}",
                out.stderr
            );
        } else {
            ctx.note(&format!("token artifacts list: {count} artifact(s)"));
        }
    }

    ctx.step("workflows with only a token");
    if let Some(out) = expect_api(
        ctx,
        &fj,
        &token,
        "api_workflows",
        caps.api_workflows,
        &ctx.actions(&dispatch, &["workflows", "--json"]),
    )
    .await?
    {
        let v = out.json()?;
        ensure!(
            v["workflows"]
                .as_array()
                .is_some_and(|w| w.iter().any(|x| x == "dispatch.yml")),
            "token workflows: {v}"
        );
    }

    ctx.step("cancel with only a token");
    if caps.api_cancel == Api::Yes {
        token_cancel(ctx, &fj, &token, &dispatch).await?;
    }
    if let Some(out) = expect_api(
        ctx,
        &fj,
        &token,
        "api_cancel",
        caps.api_cancel,
        &ctx.actions(
            &ci,
            &["cancel", "--run-index", &ci_s, "--dry-run", "--json"],
        ),
    )
    .await?
    {
        let v = out.json()?;
        ensure!(
            v["dryRun"] == true && v["url"].is_string(),
            "token cancel dry run: {v}"
        );
    }

    // A failed run: the `trigger --wait` failure from the trigger scenario.
    let failed = common::runs(ctx, &dispatch, Some("dispatch.yml"))
        .await?
        .iter()
        .find(|r| status_of(r) == "failure")
        .and_then(|r| r["runIndex"].as_i64())
        .ok_or_else(|| eyre!("no failed dispatch run to rerun"))?;
    let failed_s = failed.to_string();

    ctx.step("wait on a failed run exits non-zero with only a token");
    let out = fj
        .cmd(&ctx.actions(
            &dispatch,
            &["wait", "--run-index", &failed_s, "--timeout", "2m"],
        ))
        .env("FJ_EX_TOKEN", &token)
        .fails()
        .await?;
    ensure!(
        !out.stdout.contains(&token) && !out.stderr.contains(&token),
        "output leaked the API token"
    );
    if caps.api_jobs == Api::Yes {
        ensure!(
            out.stderr.contains("failure"),
            "token wait stderr:\n{}",
            out.stderr
        );
    } else {
        // `wait` needs the run's jobs: it must be refused like `jobs`, not fail otherwise.
        expect_refusal(ctx, "api_jobs (wait)", caps.api_jobs, &out)?;
    }

    ctx.step("rerun --failed-only with only a token raises the attempt");
    let logs_attempt2 = ctx.actions(
        &dispatch,
        &[
            "logs",
            "job",
            "--run-index",
            &failed_s,
            "--job-index",
            "0",
            "--attempt",
            "2",
        ],
    );
    if expect_api(
        ctx,
        &fj,
        &token,
        "api_rerun_failed",
        caps.api_rerun_failed,
        &ctx.actions(
            &dispatch,
            &["rerun", "--run-index", &failed_s, "--failed-only", "--json"],
        ),
    )
    .await?
    .is_some()
    {
        // Attempt 2 exists (and logs) once the rerun was scheduled and ran.
        let log = util::poll(
            "attempt 2 of the rerun job",
            Duration::from_secs(180),
            Duration::from_secs(2),
            || {
                let args = logs_attempt2.clone();
                let fj = &fj;
                let token = &token;
                async move {
                    let out = fj.cmd(&args).env("FJ_EX_TOKEN", token).output().await?;
                    Ok(
                        (out.success() && out.stdout.contains("failing as requested"))
                            .then_some(out.stdout),
                    )
                }
            },
        )
        .await?;
        ensure!(
            log.contains("fj-ex-e2e: greeting="),
            "attempt 2 log:\n{log}"
        );
        fj.cmd(&ctx.actions(
            &dispatch,
            &["wait", "--run-index", &failed_s, "--timeout", "3m"],
        ))
        .env("FJ_EX_TOKEN", &token)
        .timeout(Duration::from_secs(200))
        .fails()
        .await?;
    }
    Ok(())
}

fn has_login(list: &Value, base: &str) -> bool {
    list["logins"]
        .as_array()
        .is_some_and(|a| a.iter().any(|l| l["baseUrl"] == base))
}

/// Checks 7, 8, 9 and 12: `auth login --token-stdin` / `--token`, bad and
/// under-scoped tokens, token precedence.
pub async fn stored_tokens(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    let token = automation_token(ctx, AUTOMATION_SCOPES).await?;
    let ci = ctx.repo("ci-artifacts");
    let leak = |out: &Output| out.stdout.contains(&token) || out.stderr.contains(&token);

    ctx.step("auth login --token-stdin stores the token");
    let fj = fresh_fj(ctx, "stored-token")?;
    let out = fj
        .cmd(&["auth", "login", "--host", base, "--token-stdin"])
        .stdin(format!("{token}\n"))
        .ok()
        .await?;
    ensure!(!leak(&out), "token login echoed the token");
    let out = fj
        .cmd(&["auth", "status", "--host", base, "--json"])
        .ok()
        .await?;
    ensure!(!leak(&out), "auth status echoed the token");
    let v = out.json()?;
    ensure!(
        v["credentialKind"] == "token" && v["tokenSource"] == "stored" && v["tokenOk"] == true,
        "stored token status: {v}"
    );
    ensure!(
        v["tokenUser"] == ctx.stack.admin.username.as_str(),
        "token user: {v}"
    );
    let out = fj.cmd(&["auth", "list"]).ok().await?;
    ensure!(
        out.stdout.contains("(token)") && !leak(&out),
        "auth list:\n{}",
        out.stdout
    );
    fj.cmd(&ctx.actions(&ci, &["runs", "--latest", "--json"]))
        .json()
        .await?;

    ctx.step("a stored token beats FJ_EX_TOKEN");
    let v = fj
        .cmd(&["auth", "status", "--host", base, "--json"])
        .env("FJ_EX_TOKEN", "bogus-token-0000")
        .json()
        .await?;
    ensure!(
        v["tokenSource"] == "stored" && v["tokenOk"] == true,
        "stored vs env: {v}"
    );

    ctx.step("FJ_EX_TOKEN beats FORGEJO_TOKEN (a bogus FJ_EX_TOKEN fails)");
    let empty = fresh_fj(ctx, "precedence")?;
    let out = empty
        .cmd(&["auth", "status", "--host", base, "--json"])
        .env("FJ_EX_TOKEN", "bogus-token-0000")
        .env("FORGEJO_TOKEN", &token)
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("401"),
        "bogus FJ_EX_TOKEN should be rejected:\n{}",
        out.stderr
    );
    let v = empty
        .cmd(&["auth", "status", "--host", base, "--json"])
        .env("GITEA_TOKEN", &token)
        .json()
        .await?;
    ensure!(
        v["tokenSource"] == "env:GITEA_TOKEN" && v["tokenOk"] == true,
        "GITEA_TOKEN status: {v}"
    );

    ctx.step("a wrong token is rejected with 401 and not stored");
    let bad = fresh_fj(ctx, "bad-token")?;
    let out = bad
        .cmd(&["auth", "login", "--host", base, "--token-stdin"])
        .stdin("0123456789abcdef0123456789abcdef01234567\n")
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("401"),
        "bad token stderr:\n{}",
        out.stderr
    );
    let list = bad.cmd(&["auth", "list", "--json"]).json().await?;
    ensure!(
        !has_login(&list, base),
        "a rejected token was stored: {list}"
    );

    ctx.step("a token without read:user is stored with a warning");
    let narrow_token = automation_token(ctx, &["read:repository", "write:repository"]).await?;
    let narrow = fresh_fj(ctx, "narrow-token")?;
    let out = narrow
        .cmd(&["auth", "login", "--host", base, "--token-stdin"])
        .stdin(format!("{narrow_token}\n"))
        .ok()
        .await?;
    ensure!(
        out.stderr.contains("read:user"),
        "missing read:user warning:\n{}",
        out.stderr
    );
    let v = narrow
        .cmd(&["auth", "status", "--host", base, "--json"])
        .json()
        .await?;
    ensure!(
        v["credentialKind"] == "token" && v["tokenOk"] == true,
        "narrow token status: {v}"
    );
    narrow
        .cmd(&ctx.actions(&ci, &["runs", "--latest", "--json"]))
        .json()
        .await?;

    ctx.step("--token on the command line warns about the process list");
    let argv = fresh_fj(ctx, "argv-token")?;
    let out = argv
        .cmd(&["auth", "login", "--host", base, "--token", &token])
        .ok()
        .await?;
    ensure!(
        out.stderr.contains("process list"),
        "no --token warning:\n{}",
        out.stderr
    );
    ensure!(!leak(&out), "--token login echoed the token");
    Ok(())
}

/// Checks 10 and 11: `auth login --session-cookie-stdin`, then an expired session.
pub async fn session_import(ctx: &Ctx<'_>) -> Result<()> {
    let base = ctx.base();
    let admin = &ctx.stack.admin;
    let cookie_name = ctx.caps().session_cookie;
    let ci = ctx.repo("ci-artifacts");

    ctx.step(&format!(
        "log in through the web form and take the `{cookie_name}` cookie"
    ));
    let web = ui::WebSession::login(base, &admin.username, &admin.password).await?;
    let value = web
        .cookie(cookie_name)
        .ok_or_else(|| eyre!("web login set no {cookie_name} cookie"))?;

    ctx.step("auth login --session-cookie-stdin");
    let fj = fresh_fj(ctx, "session")?;
    let out = fj
        .cmd(&["auth", "login", "--host", base, "--session-cookie-stdin"])
        .stdin(format!("{cookie_name}={value}\n"))
        .ok()
        .await?;
    ensure!(
        !out.stdout.contains(&value) && !out.stderr.contains(&value),
        "session import echoed the cookie"
    );
    let v = fj
        .cmd(&["auth", "status", "--host", base, "--json", "--no-relogin"])
        .json()
        .await?;
    ensure!(
        v["credentialKind"] == "session" && v["sessionOk"] == true,
        "session status: {v}"
    );
    ensure!(
        v["tokenSource"].is_null(),
        "no token should be involved: {v}"
    );
    let v = fj
        .cmd(&ctx.actions(&ci, &["runs", "--latest", "--json"]))
        .json()
        .await?;
    ensure!(
        v["runs"].as_array().is_some_and(|r| r.len() == 1),
        "session runs: {v}"
    );

    ctx.step("log the browser session out server-side");
    web.logout().await?;

    ctx.step("an expired imported session fails clearly and stays stored");
    let out = fj
        .cmd(&ctx.actions(&ci, &["runs", "--latest"]))
        .fails()
        .await?;
    ensure!(
        out.stderr.contains("has expired"),
        "expired session stderr:\n{}",
        out.stderr
    );
    let list = fj.cmd(&["auth", "list", "--json"]).json().await?;
    ensure!(
        has_login(&list, base),
        "expired session entry was removed: {list}"
    );

    ctx.step("with FJ_EX_TOKEN the same command falls back to the API");
    let token = automation_token(ctx, AUTOMATION_SCOPES).await?;
    let out = fj
        .cmd(&ctx.actions(&ci, &["runs", "--latest", "--json"]))
        .env("FJ_EX_TOKEN", &token)
        .ok()
        .await?;
    ensure!(
        out.stderr.contains("falling back to the REST API"),
        "no fallback warning:\n{}",
        out.stderr
    );
    ensure!(
        out.json()?["runs"].as_array().is_some_and(|r| r.len() == 1),
        "fallback runs"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run_url_number;

    #[test]
    fn run_url_number_reads_the_runs_segment() {
        assert_eq!(run_url_number("http://h/o/r/actions/runs/12"), Some(12));
        assert_eq!(
            run_url_number("http://h/o/r/actions/runs/3/jobs/0"),
            Some(3)
        );
        assert_eq!(run_url_number("http://h/o/r/actions"), None);
    }
}
