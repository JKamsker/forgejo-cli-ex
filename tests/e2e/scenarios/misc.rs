//! `smoke-test` and host/repo inference from a cloned repository's remote.

use eyre::{ensure, Result};

use super::Ctx;
use crate::e2e::harness::git::Git;

pub async fn smoke_test(ctx: &Ctx<'_>) -> Result<()> {
    let ci = ctx.repo("ci-artifacts");
    let out_dir = ctx.scratch.join("smoke");

    ctx.step("smoke-test");
    let out = ctx
        .fj
        .cmd(&[
            "smoke-test",
            "--host",
            ctx.base(),
            "--repo",
            &ci,
            "--out-dir",
            &out_dir.to_string_lossy(),
        ])
        .ok()
        .await?;
    ensure!(
        out.stdout.lines().any(|l| l.trim() == "OK"),
        "smoke-test output:\n{}",
        out.stdout
    );
    ensure!(
        out.stdout.contains("artifactCount=1"),
        "smoke-test artifacts:\n{}",
        out.stdout
    );

    ctx.step("actions smoke-test");
    let out = ctx
        .fj
        .cmd(&ctx.actions(
            &ci,
            &["smoke-test", "--out-dir", &out_dir.to_string_lossy()],
        ))
        .ok()
        .await?;
    ensure!(
        out.stdout.lines().any(|l| l.trim() == "OK"),
        "actions smoke-test output:\n{}",
        out.stdout
    );
    Ok(())
}

pub async fn remote_inference(ctx: &Ctx<'_>) -> Result<()> {
    let git = Git::anonymous(ctx.base());
    let ci = ctx.stack.repo("ci-artifacts");
    let pulls = ctx.stack.repo("pulls-demo");

    ctx.step("clone demo repositories with the system git");
    let ci_dir = ctx.scratch.join("clones").join("ci-artifacts");
    let pulls_dir = ctx.scratch.join("clones").join("pulls-demo");
    std::fs::create_dir_all(ci_dir.parent().unwrap())?;
    let parent = ci_dir.parent().unwrap().to_path_buf();
    git.run(
        &parent,
        &["clone", "-q", &ci.clone_url, &ci_dir.to_string_lossy()],
    )?;
    git.run(
        &parent,
        &[
            "clone",
            "-q",
            &pulls.clone_url,
            &pulls_dir.to_string_lossy(),
        ],
    )?;

    ctx.step("actions runs infers host and repo from origin");
    let v = ctx
        .fj
        .cmd(&["actions", "runs", "--latest", "--json"])
        .cwd(&ci_dir)
        .json()
        .await?;
    ensure!(v["baseUrl"] == ctx.base(), "inferred baseUrl: {v}");
    ensure!(v["repo"] == ci.full_name.as_str(), "inferred repo: {v}");
    ensure!(
        v["runs"].as_array().is_some_and(|r| r.len() == 1),
        "inferred runs: {v}"
    );

    ctx.step("a different remote name via --remote");
    git.run(&ci_dir, &["remote", "rename", "origin", "forge"])?;
    let out = ctx
        .fj
        .cmd(&["actions", "runs", "--latest", "--json"])
        .cwd(&ci_dir)
        .output()
        .await?;
    ensure!(
        !out.success() || out.json().is_ok_and(|v| v["repo"] == ci.full_name.as_str()),
        "unexpected output without origin:\n{}\n{}",
        out.stdout,
        out.stderr
    );
    let v = ctx
        .fj
        .cmd(&["actions", "--remote", "forge", "runs", "--latest", "--json"])
        .cwd(&ci_dir)
        .json()
        .await?;
    ensure!(
        v["repo"] == ci.full_name.as_str() && v["baseUrl"] == ctx.base(),
        "--remote forge: {v}"
    );

    ctx.step("pulls list and auth status infer the target too");
    let v = ctx
        .fj
        .cmd(&["pulls", "list", "--state", "all", "--json"])
        .cwd(&pulls_dir)
        .json()
        .await?;
    ensure!(
        v["repo"] == pulls.full_name.as_str(),
        "pulls inference: {v}"
    );
    let v = ctx
        .fj
        .cmd(&["auth", "status", "--json"])
        .cwd(&pulls_dir)
        .json()
        .await?;
    ensure!(
        v["baseUrl"] == ctx.base() && v["sessionOk"] == true,
        "auth status inference: {v}"
    );
    Ok(())
}
