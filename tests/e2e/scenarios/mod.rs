//! `fj-ex` scenarios run against one seeded stack.
//!
//! Scenarios return `Result` (they use `ensure!` rather than `assert!`), so
//! one failing scenario does not hide the results of the independent ones.

mod actions_runs;
mod artifacts;
mod auth;
mod common;
mod control;
mod dispatch;
mod logs;
mod misc;
mod pulls;
mod runners;
mod settings;
mod tokens;
mod watch;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eyre::{eyre, Result};

use super::cli::FjEx;
use super::harness::stack::Stack;
use super::harness::targets::Caps;
use super::harness::util;

pub struct Ctx<'a> {
    pub stack: &'a Stack,
    /// `fj-ex` logged in as the admin (after the auth scenario).
    pub fj: FjEx,
    /// `fj-ex` logged in as the reviewer, without an `fj` API token (Basic creds path).
    pub reviewer: FjEx,
    /// Scratch space for downloads and clones.
    pub scratch: PathBuf,
    /// Per-run random suffix for unique values.
    pub suffix: String,
    notes: Mutex<Vec<String>>,
}

impl<'a> Ctx<'a> {
    fn new(stack: &'a Stack) -> Result<Self> {
        let root = stack.work_dir.join("fj-ex");
        let tag = stack.target.id;
        Ok(Self {
            stack,
            fj: FjEx::new(&root.join("appdata-admin"), tag)?,
            reviewer: FjEx::new(&root.join("appdata-reviewer"), tag)?,
            scratch: {
                let p = root.join("scratch");
                std::fs::create_dir_all(&p)?;
                p
            },
            suffix: util::unique_suffix(),
            notes: Mutex::new(Vec::new()),
        })
    }

    pub fn tag(&self) -> &str {
        self.stack.target.id
    }

    pub fn base(&self) -> &str {
        &self.stack.base_url
    }

    pub fn caps(&self) -> Caps {
        self.stack.target.caps
    }

    /// `owner/name` of a seeded demo repository.
    pub fn repo(&self, name: &str) -> String {
        self.stack.repo(name).full_name.clone()
    }

    /// `actions --host <base> --repo <repo> <args...>`.
    pub fn actions(&self, repo: &str, args: &[&str]) -> Vec<String> {
        let mut v = vec![
            "actions".to_string(),
            "--host".into(),
            self.base().into(),
            "--repo".into(),
            repo.into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    /// `pulls --host <base> --repo <repo> <args...>`.
    pub fn pulls(&self, repo: &str, args: &[&str]) -> Vec<String> {
        let mut v = vec![
            "pulls".to_string(),
            "--host".into(),
            self.base().into(),
            "--repo".into(),
            repo.into(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    pub fn note(&self, msg: &str) {
        eprintln!("[{}]   note: {msg}", self.tag());
        self.notes.lock().unwrap().push(msg.to_string());
    }

    /// Record (loudly) that a check is skipped because the target lacks a capability.
    pub fn skip_capability(&self, check: &str, capability: &str) {
        self.note(&format!(
            "SKIPPED {check}: capability `{capability}` is off for {} ({})",
            self.stack.target.label(),
            Caps::explain(capability)
        ));
    }

    pub fn step(&self, msg: &str) {
        eprintln!("[{}]   - {msg}", self.tag());
    }
}

struct Outcome {
    name: &'static str,
    elapsed: Duration,
    error: Option<String>,
}

async fn scenario<F: Future<Output = Result<()>>>(
    ctx: &Ctx<'_>,
    name: &'static str,
    fut: F,
) -> Outcome {
    eprintln!("[{}] ▶ scenario {name}", ctx.tag());
    let started = Instant::now();
    let result = fut.await;
    let elapsed = started.elapsed();
    match &result {
        Ok(()) => eprintln!(
            "[{}] ✔ scenario {name} ({:.1}s)",
            ctx.tag(),
            elapsed.as_secs_f64()
        ),
        Err(e) => eprintln!(
            "[{}] ✘ scenario {name} ({:.1}s): {e:?}",
            ctx.tag(),
            elapsed.as_secs_f64()
        ),
    }
    Outcome {
        name,
        elapsed,
        error: result.err().map(|e| format!("{e:?}")),
    }
}

pub async fn run_all(stack: &Stack) -> Result<()> {
    let ctx = Ctx::new(stack)?;
    let mut outcomes = Vec::new();

    // Everything else needs a logged-in fj-ex, so a failure here aborts.
    let first = scenario(&ctx, "auth-login", auth::login(&ctx)).await;
    let abort = first.error.is_some();
    outcomes.push(first);

    if !abort {
        outcomes.push(scenario(&ctx, "auth-2fa", auth::two_factor(&ctx)).await);
        outcomes.push(scenario(&ctx, "token", tokens::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "runners", runners::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "secrets-variables", settings::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "runs-jobs-wait", actions_runs::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "logs", logs::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "artifacts", artifacts::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "smoke-test", misc::smoke_test(&ctx)).await);
        outcomes.push(scenario(&ctx, "remote-inference", misc::remote_inference(&ctx)).await);
        outcomes.push(scenario(&ctx, "trigger", dispatch::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "follow-watch", watch::follow_and_watch(&ctx)).await);
        outcomes.push(scenario(&ctx, "cancel", control::cancel(&ctx)).await);
        outcomes.push(scenario(&ctx, "rerun", control::rerun(&ctx)).await);
        outcomes.push(scenario(&ctx, "runner-queue", runners::waiting_queue(&ctx)).await);
        outcomes.push(scenario(&ctx, "pulls", pulls::run(&ctx)).await);
        outcomes.push(scenario(&ctx, "auth-logout", auth::logout(&ctx)).await);
    }

    let tag = ctx.tag();
    eprintln!(
        "\n[{tag}] ===== scenario summary ({}) =====",
        stack.target.label()
    );
    for o in &outcomes {
        eprintln!(
            "[{tag}]   {:<4} {:<18} {:>6.1}s",
            if o.error.is_none() { "ok" } else { "FAIL" },
            o.name,
            o.elapsed.as_secs_f64()
        );
    }
    for n in ctx.notes.lock().unwrap().iter() {
        eprintln!("[{tag}]   note: {n}");
    }

    let failed: Vec<&Outcome> = outcomes.iter().filter(|o| o.error.is_some()).collect();
    if failed.is_empty() {
        return Ok(());
    }
    let mut msg = format!(
        "{} scenario(s) failed on {}:",
        failed.len(),
        stack.target.label()
    );
    for o in failed {
        msg.push_str(&format!(
            "\n\n## {}\n{}",
            o.name,
            o.error.as_deref().unwrap_or_default()
        ));
    }
    Err(eyre!(msg))
}
