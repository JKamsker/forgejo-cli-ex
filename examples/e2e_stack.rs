//! Manual E2E stacks: boot one Forgejo/Gitea target with its runner and the
//! seeded demo repositories, then point `fj-ex` at it by hand.
//!
//! Usually invoked through `tests/e2e/up.sh` and `tests/e2e/down.sh`:
//!
//! ```sh
//! cargo run --example e2e_stack -- up forgejo-17
//! cargo run --example e2e_stack -- list
//! cargo run --example e2e_stack -- down <stack-id> | --all
//! cargo run --example e2e_stack -- targets
//! ```
//!
//! It shares `tests/e2e/harness` with the `e2e_docker` tests, so a manual
//! stack is booted and seeded exactly like the automated one.

#[path = "../tests/e2e/harness/mod.rs"]
mod harness;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use eyre::{eyre, Result};

use harness::stack::{write_fj_keys_json, write_private, Stack, StackOptions};
use harness::{docker, targets};

const USAGE: &str = "\
usage: e2e_stack <command>

commands:
  up <target> [--work-root DIR]   start a server + runner and seed the demo repositories
  list                            list running stacks
  down <stack-id>... | --all      remove stacks (containers and their work directory)
  targets                         list the available targets

The stack keeps running until `down`. Work directories default to
target/fjex-e2e/<stack-id> (stack.env, an isolated fj-ex APPDATA, runner config, logs).";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("up") => up(&args[1..]).await,
        Some("list") => list(),
        Some("down") => down(&args[1..]),
        Some("targets") => {
            print_targets();
            Ok(())
        }
        Some("-h" | "--help" | "help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(eyre!("unknown command {other:?}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

fn default_work_root() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"))
        .join("fjex-e2e")
}

fn print_targets() {
    println!(
        "{:<12} {:<8} {:<9} {:<40} {:<40} workflows",
        "id", "family", "version", "image", "runner"
    );
    for t in targets::TARGETS {
        println!(
            "{:<12} {:<8} {:<9} {:<40} {:<40} {}",
            t.id,
            t.family.name(),
            t.version,
            t.image,
            t.runner_image,
            t.workflow_dir
        );
    }
}

async fn up(args: &[String]) -> Result<()> {
    let mut target_id = None;
    let mut work_root = default_work_root();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--work-root" => {
                work_root = PathBuf::from(
                    it.next()
                        .ok_or_else(|| eyre!("--work-root needs a value"))?,
                );
            }
            other if target_id.is_none() => target_id = Some(other.to_string()),
            other => return Err(eyre!("unexpected argument {other:?}")),
        }
    }
    let target_id = target_id.ok_or_else(|| eyre!("missing <target>\n\n{USAGE}"))?;
    let target = targets::find(&target_id).ok_or_else(|| {
        let ids: Vec<&str> = targets::TARGETS.iter().map(|t| t.id).collect();
        eyre!(
            "unknown target {target_id:?}; available: {}",
            ids.join(", ")
        )
    })?;
    if !docker::available() {
        return Err(eyre!("the docker daemon is not reachable"));
    }

    let mut stack = Stack::up(
        target,
        StackOptions {
            keep: true,
            work_root: Some(work_root),
        },
    )
    .await?;
    stack.mark_ok();

    let appdata = stack.work_dir.join("appdata");
    write_fj_keys_json(&appdata, &stack.base_url, &stack.admin.token)?;
    let env_path = stack.work_dir.join("stack.env");
    write_private(&env_path, &stack.env_file_contents(&appdata))?;

    let base = &stack.base_url;
    let ci = stack.repo("ci-artifacts").full_name.clone();
    println!();
    println!(
        "Stack {} is up: {} (server {})",
        stack.id,
        target.label(),
        stack.server_version
    );
    println!("  URL       {base}");
    println!(
        "  admin     {} / {}",
        stack.admin.username, stack.admin.password
    );
    println!(
        "  reviewer  {} / {}",
        stack.reviewer.username, stack.reviewer.password
    );
    println!(
        "  repos     {}",
        stack
            .repos
            .iter()
            .map(|r| r.full_name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("  env file  {}", env_path.display());
    println!();
    println!(
        "Use fj-ex with an isolated credential store (keys.json already holds an admin API token):"
    );
    println!("  export APPDATA={}", appdata.display());
    println!(
        "  printf '%s\\n' '{}' | fj-ex auth login --host {base} --username {} --password-stdin",
        stack.admin.password, stack.admin.username
    );
    println!("  fj-ex actions --host {base} --repo {ci} runs");
    println!(
        "  git clone {}/{ci}.git && cd ci-artifacts && fj-ex actions runs --latest",
        base
    );
    println!();
    println!("Tear down:");
    println!("  tests/e2e/down.sh {}", stack.id);
    Ok(())
}

fn list() -> Result<()> {
    let containers = docker::list_stack_containers();
    if containers.is_empty() {
        println!("No fj-ex e2e stacks.");
        return Ok(());
    }
    println!(
        "{:<34} {:<11} {:<7} {:<26} ports",
        "stack", "target", "role", "status"
    );
    for c in containers {
        println!(
            "{:<34} {:<11} {:<7} {:<26} {}",
            c.stack, c.target, c.role, c.status, c.ports
        );
    }
    Ok(())
}

fn down(args: &[String]) -> Result<()> {
    let containers = docker::list_stack_containers();
    let stacks: Vec<String> = if args.iter().any(|a| a == "--all") {
        let mut ids: Vec<String> = containers.iter().map(|c| c.stack.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    } else if args.is_empty() {
        return Err(eyre!("pass one or more stack ids, or --all (see `list`)"));
    } else {
        args.to_vec()
    };
    if stacks.is_empty() {
        println!("No fj-ex e2e stacks.");
    }
    for id in stacks {
        if !id.starts_with(docker::NAME_PREFIX) {
            return Err(eyre!("{id:?} is not an fj-ex e2e stack id"));
        }
        let work_dirs: Vec<String> = containers
            .iter()
            .filter(|c| c.stack == id && !c.work_dir.is_empty())
            .map(|c| c.work_dir.clone())
            .collect();
        let removed = docker::remove_stack(&id);
        println!("{id}: removed {removed} container(s)");
        for dir in work_dirs.iter().take(1) {
            let path = Path::new(dir);
            // Only delete directories the harness created for this stack.
            if path.file_name().is_some_and(|n| n == id.as_str()) && path.is_dir() {
                std::fs::remove_dir_all(path)?;
                println!("{id}: removed {}", path.display());
            }
        }
    }
    Ok(())
}
