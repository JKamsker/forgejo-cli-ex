//! Docker E2E suite: one server container (+ runner) per target, seeded with
//! the demo repositories in `fixtures/repos`, then every `fj-ex` scenario runs
//! against it. See `tests/e2e/README.md`.

pub mod cli;
pub mod harness;
mod scenarios;

use std::time::Instant;

use eyre::{eyre, Result};

use harness::stack::{Stack, StackOptions};
use harness::{docker, git, targets, util};

/// Entry point of every per-target test.
pub async fn run_target(id: &str) -> Result<()> {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping e2e {id}: requires Linux (job containers use Docker host networking)");
        return Ok(());
    }
    if !util::env_flag("FJ_EX_E2E") {
        eprintln!("skipping e2e {id}: set FJ_EX_E2E=1 to enable");
        return Ok(());
    }
    if !targets::is_selected(id)? {
        eprintln!("skipping e2e {id}: not selected by FJ_EX_E2E_TARGET");
        return Ok(());
    }
    // Explicitly enabled: a missing prerequisite is a failure, not a skip.
    if !docker::available() {
        return Err(eyre!("FJ_EX_E2E=1 but the docker daemon is not reachable"));
    }
    if !git::Git::available() {
        return Err(eyre!("FJ_EX_E2E=1 but `git` is not installed"));
    }
    let target = targets::find(id).ok_or_else(|| eyre!("unknown e2e target {id}"))?;

    let started = Instant::now();
    let mut stack = Stack::up(
        target,
        StackOptions {
            keep: util::env_flag("FJ_EX_E2E_KEEP"),
            work_root: None,
        },
    )
    .await?;
    let boot = started.elapsed();

    let result = scenarios::run_all(&stack).await;
    if result.is_ok() {
        stack.mark_ok();
    }
    eprintln!(
        "[{id}] {} {} in {:.1}s (stack boot + seeding {:.1}s)",
        target.label(),
        if result.is_ok() { "PASSED" } else { "FAILED" },
        started.elapsed().as_secs_f64(),
        boot.as_secs_f64()
    );
    result
}
