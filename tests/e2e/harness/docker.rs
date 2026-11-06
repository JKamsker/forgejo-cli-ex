//! Thin wrappers around the `docker` CLI.

use std::process::{Command, Stdio};

use eyre::{eyre, Context, Result};

/// Label carried by every container the harness creates (server, runner and,
/// through the runner config, job containers).
pub const LABEL: &str = "fjex-e2e";
/// Label whose value is the stack id; `down` removes by this label.
pub const STACK_LABEL: &str = "fjex-e2e.stack";
pub const TARGET_LABEL: &str = "fjex-e2e.target";
/// Host work directory of the stack (seed trees, runner config, logs).
pub const WORK_DIR_LABEL: &str = "fjex-e2e.workdir";
/// Every container name starts with this prefix.
pub const NAME_PREFIX: &str = "fjex-e2e-";

pub fn available() -> bool {
    Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run `docker <args>` and return trimmed stdout. Fails with stderr on a non-zero exit.
pub fn run(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .wrap_err_with(|| format!("failed to spawn docker {}", describe(args)))?;
    if !out.status.success() {
        return Err(eyre!(
            "docker {} failed ({}):\n{}{}",
            describe(args),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like [`run`], but never echoes arguments (for commands that may carry secrets).
pub fn run_quiet(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .wrap_err("failed to spawn docker")?;
    if !out.status.success() {
        return Err(eyre!(
            "docker {} failed ({}):\n{}",
            args.first().copied().unwrap_or_default(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn describe(args: &[&str]) -> String {
    // Keep error messages short: env values (-e K=V) are elided.
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for a in args {
        if redact_next {
            out.push(a.split('=').next().unwrap_or_default().to_string() + "=…");
            redact_next = false;
            continue;
        }
        redact_next = *a == "-e";
        out.push((*a).to_string());
    }
    out.join(" ")
}

/// Combined stdout+stderr of `docker logs`, optionally limited to the last lines.
pub fn logs(container: &str, tail: Option<usize>) -> String {
    let tail_s = tail.map(|n| n.to_string());
    let mut args = vec!["logs"];
    if let Some(t) = tail_s.as_deref() {
        args.extend(["--tail", t]);
    }
    args.push(container);
    match Command::new("docker").args(&args).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        }
        Err(e) => format!("<failed to read docker logs for {container}: {e}>"),
    }
}

pub fn is_running(container: &str) -> bool {
    run(&["inspect", "-f", "{{.State.Running}}", container]).is_ok_and(|s| s == "true")
}

/// Container ids (all states) carrying `label=value`.
pub fn containers_with_label(label: &str, value: Option<&str>) -> Vec<String> {
    let filter = match value {
        Some(v) => format!("label={label}={v}"),
        None => format!("label={label}"),
    };
    run(&["ps", "-aq", "--filter", &filter])
        .map(|s| {
            s.lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Force-remove every container (and its anonymous volumes) of a stack.
pub fn remove_stack(stack_id: &str) -> usize {
    let ids = containers_with_label(STACK_LABEL, Some(stack_id));
    if ids.is_empty() {
        return 0;
    }
    let mut args = vec!["rm", "-f", "-v"];
    args.extend(ids.iter().map(String::as_str));
    let _ = Command::new("docker")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    ids.len()
}

/// One harness container as reported by `docker ps`.
#[derive(Clone, Debug)]
pub struct StackContainer {
    pub stack: String,
    pub target: String,
    pub role: String,
    pub name: String,
    pub status: String,
    pub ports: String,
    pub work_dir: String,
}

/// All containers created by the harness (any stack, any state).
pub fn list_stack_containers() -> Vec<StackContainer> {
    let filter = format!("label={LABEL}");
    let format = [
        format!("{{{{.Label \"{STACK_LABEL}\"}}}}"),
        format!("{{{{.Label \"{TARGET_LABEL}\"}}}}"),
        "{{.Label \"fjex-e2e.role\"}}".to_string(),
        "{{.Names}}".to_string(),
        "{{.Status}}".to_string(),
        "{{.Ports}}".to_string(),
        format!("{{{{.Label \"{WORK_DIR_LABEL}\"}}}}"),
    ]
    .join("\t");
    run(&["ps", "-a", "--filter", &filter, "--format", &format])
        .map(|s| {
            s.lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    (f.len() == 7).then(|| StackContainer {
                        stack: f[0].to_string(),
                        target: f[1].to_string(),
                        role: f[2].to_string(),
                        name: f[3].to_string(),
                        status: f[4].to_string(),
                        ports: f[5].to_string(),
                        work_dir: f[6].to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
