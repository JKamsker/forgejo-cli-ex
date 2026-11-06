//! `pulls list/create/comment/comments/review/reviews/merge`.

use std::time::Duration;

use eyre::{ensure, eyre, Result};
use serde_json::Value;

use super::Ctx;
use crate::e2e::harness::util;

const HEAD: &str = "feature/greeting";

pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let repo = ctx.repo("pulls-demo");
    let title = format!("feat: friendlier greeting ({})", ctx.suffix);

    ctx.step("pulls list --json starts empty");
    let v = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["list", "--json"]))
        .json()
        .await?;
    ensure!(
        v["state"] == "open" && v["pulls"].as_array().is_some_and(|p| p.is_empty()),
        "initial list: {v}"
    );

    ctx.step("pulls create");
    let out = ctx
        .fj
        .cmd(&ctx.pulls(
            &repo,
            &[
                "create", "--head", HEAD, "--base", "main", "--title", &title, "--body", "Demo PR",
            ],
        ))
        .ok()
        .await?;
    let index: u64 = out
        .stdout
        .split_whitespace()
        .find_map(|w| w.strip_prefix('#').and_then(|n| n.parse().ok()))
        .ok_or_else(|| eyre!("no PR number in {:?}", out.stdout))?;
    let index_s = index.to_string();

    ctx.step("pulls list --json / table");
    let v = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["list", "--json", "--limit", "5"]))
        .json()
        .await?;
    let pr = v["pulls"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["number"] == index))
        .ok_or_else(|| eyre!("PR #{index} not listed: {v}"))?
        .clone();
    ensure!(
        pr["title"] == title.as_str() && pr["head"]["ref"] == HEAD,
        "listed PR: {pr}"
    );
    let head_sha = pr["head"]["sha"]
        .as_str()
        .ok_or_else(|| eyre!("PR has no head sha: {pr}"))?
        .to_string();
    let out = ctx.fj.cmd(&ctx.pulls(&repo, &["list"])).ok().await?;
    ensure!(out.stdout.contains(&title), "list table:\n{}", out.stdout);

    ctx.step("pulls comment (--body, --body-file -, --body-file <path>)");
    let c1 = ctx
        .fj
        .cmd(&ctx.pulls(
            &repo,
            &[
                "comment",
                "--index",
                &index_s,
                "--body",
                "first comment",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(c1["body"] == "first comment", "comment json: {c1}");
    let out = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["comment", "--index", &index_s, "--body-file", "-"]))
        .stdin("comment from stdin\nsecond line\n")
        .ok()
        .await?;
    ensure!(
        out.stdout.contains(&format!("on pull request #{index}")),
        "comment text:\n{}",
        out.stdout
    );
    let body_file = ctx.scratch.join("review.md");
    std::fs::write(&body_file, "## Review\n\nLooks good from a file.\n")?;
    let body_path = body_file.to_string_lossy().to_string();
    ctx.fj
        .cmd(&ctx.pulls(
            &repo,
            &["comment", "--index", &index_s, "--body-file", &body_path],
        ))
        .ok()
        .await?;

    ctx.step("pulls comments --json");
    let v = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["comments", "--index", &index_s, "--json"]))
        .json()
        .await?;
    let bodies = bodies(&v["comments"]);
    for expected in [
        "first comment",
        "comment from stdin\nsecond line",
        "Looks good from a file.",
    ] {
        ensure!(
            bodies.iter().any(|b| b.contains(expected)),
            "comment {expected:?} missing: {bodies:?}"
        );
    }
    let out = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["comments", "--index", &index_s]))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("first comment"),
        "comments table:\n{}",
        out.stdout
    );

    ctx.step("pulls review (default COMMENT event) by the author");
    let r = ctx
        .fj
        .cmd(&ctx.pulls(
            &repo,
            &[
                "review",
                "--index",
                &index_s,
                "--body-file",
                &body_path,
                "--commit",
                &head_sha,
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(r["state"] == "COMMENT", "review json: {r}");

    ctx.step("pulls review --event approve by the reviewer (UI basic creds)");
    let r = ctx
        .reviewer
        .cmd(&ctx.pulls(
            &repo,
            &[
                "review",
                "--index",
                &index_s,
                "--body",
                "Approved by e2e",
                "--event",
                "approve",
                "--json",
            ],
        ))
        .json()
        .await?;
    ensure!(r["state"] == "APPROVED", "approve json: {r}");
    ensure!(
        r["user"]["login"] == ctx.stack.reviewer.username.as_str(),
        "review author: {r}"
    );

    ctx.step("pulls reviews --json");
    let v = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["reviews", "--index", &index_s, "--json"]))
        .json()
        .await?;
    let states: Vec<&str> = v["reviews"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["state"].as_str())
        .collect();
    ensure!(
        states.contains(&"COMMENT") && states.contains(&"APPROVED"),
        "review states: {states:?}"
    );
    let out = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["reviews", "--index", &index_s]))
        .ok()
        .await?;
    ensure!(
        out.stdout.contains("APPROVED"),
        "reviews table:\n{}",
        out.stdout
    );

    ctx.step("wait until the server finished its mergeability check");
    // Right after creation the server answers merges with "Please try again later".
    util::poll(
        "PR to become mergeable",
        Duration::from_secs(90),
        Duration::from_secs(1),
        || {
            let args = ctx.pulls(&repo, &["list", "--json"]);
            async move {
                let v = ctx.fj.cmd(&args).json().await?;
                let mergeable = v["pulls"]
                    .as_array()
                    .and_then(|p| p.iter().find(|p| p["number"] == index))
                    .is_some_and(|p| p["mergeable"] == true);
                Ok(mergeable.then_some(()))
            }
        },
    )
    .await?;

    ctx.step("pulls merge refuses a stale head commit");
    let stale = "0000000000000000000000000000000000000000";
    let out = ctx
        .fj
        .cmd(&ctx.pulls(
            &repo,
            &[
                "merge",
                "--index",
                &index_s,
                "--head-commit",
                stale,
                "--title",
                &title,
            ],
        ))
        .fails()
        .await?;
    ensure!(
        !out.stderr.contains("try again later"),
        "stale merge hit a transient state:\n{}",
        out.stderr
    );

    ctx.step("pulls merge with the current head commit");
    // fj-ex itself retries the transient 405 "Please try again later" that the
    // server answers while it re-checks mergeability (e.g. after the stale merge).
    let out = ctx
        .fj
        .cmd(&ctx.pulls(
            &repo,
            &[
                "merge",
                "--index",
                &index_s,
                "--head-commit",
                &head_sha,
                "--title",
                &title,
            ],
        ))
        .ok()
        .await?;
    if out.stderr.contains("merged on attempt") {
        ctx.note(
            out.stderr
                .lines()
                .find(|l| l.contains("merged on attempt"))
                .unwrap_or_default()
                .trim_start_matches("note: "),
        );
    }
    ensure!(
        out.stdout
            .contains(&format!("Merged pull request #{index}")),
        "merge:\n{}",
        out.stdout
    );
    let v = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["list", "--state", "closed", "--json"]))
        .json()
        .await?;
    let merged = v["pulls"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["number"] == index))
        .ok_or_else(|| eyre!("merged PR not in closed list: {v}"))?;
    ensure!(merged["merged"] == true, "merged flag: {merged}");
    let v = ctx
        .fj
        .cmd(&ctx.pulls(&repo, &["list", "--state", "open", "--json"]))
        .json()
        .await?;
    ensure!(
        v["pulls"].as_array().is_some_and(|p| p.is_empty()),
        "open list after merge: {v}"
    );
    Ok(())
}

fn bodies(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["body"].as_str().map(str::to_string))
        .collect()
}
