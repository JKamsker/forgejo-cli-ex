//! Driver for the `fj-ex` binary under test.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use eyre::{eyre, Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// `fj-ex` with an isolated credential store (`APPDATA`).
#[derive(Clone)]
pub struct FjEx {
    bin: PathBuf,
    appdata: PathBuf,
    /// Prefix for log lines, e.g. `forgejo-17`.
    tag: String,
}

#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.stdout)
            .wrap_err_with(|| format!("stdout is not JSON:\n{}", clip(&self.stdout)))
    }
}

pub fn bin_path() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_fj-ex") {
        return Ok(PathBuf::from(path));
    }
    Err(eyre!(
        "CARGO_BIN_EXE_fj-ex is not set; run through `cargo test`"
    ))
}

impl FjEx {
    pub fn new(appdata: &Path, tag: &str) -> Result<Self> {
        std::fs::create_dir_all(appdata)?;
        Ok(Self {
            bin: bin_path()?,
            appdata: appdata.to_path_buf(),
            tag: tag.to_string(),
        })
    }

    pub fn appdata(&self) -> &Path {
        &self.appdata
    }

    pub fn cmd<S: AsRef<str>>(&self, args: &[S]) -> Invocation<'_> {
        Invocation {
            fj: self,
            args: args.iter().map(|a| a.as_ref().to_string()).collect(),
            stdin: None,
            cwd: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

pub struct Invocation<'a> {
    fj: &'a FjEx,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
    cwd: Option<PathBuf>,
    timeout: Duration,
}

impl Invocation<'_> {
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn display(&self) -> String {
        let mut s = String::from("fj-ex");
        for a in &self.args {
            if a.is_empty() || a.contains(char::is_whitespace) {
                s.push_str(&format!(" '{a}'"));
            } else {
                s.push(' ');
                s.push_str(a);
            }
        }
        s
    }

    /// Run and return the output regardless of the exit status.
    pub async fn output(self) -> Result<Output> {
        let shown = self.display();
        let started = Instant::now();
        let mut cmd = tokio::process::Command::new(&self.fj.bin);
        cmd.args(&self.args)
            .env("APPDATA", &self.fj.appdata)
            .env("NO_COLOR", "1")
            // Never let the developer's environment leak into the test.
            .env_remove("FJ_FALLBACK_HOST")
            .env_remove("FJ_USER")
            .env_remove("FJ_PASS")
            .env_remove("FJ_OTP")
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .wrap_err_with(|| format!("failed to spawn {shown}"))?;
        if let Some(input) = self.stdin {
            let mut pipe = child
                .stdin
                .take()
                .ok_or_else(|| eyre!("stdin pipe missing"))?;
            pipe.write_all(&input).await?;
            drop(pipe);
        }
        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(r) => r.wrap_err_with(|| format!("failed to wait for {shown}"))?,
            Err(_) => return Err(eyre!("`{shown}` did not finish within {:?}", self.timeout)),
        };
        let output = Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        eprintln!(
            "[{}]   $ {shown}  -> exit {} in {:.1}s",
            self.fj.tag,
            output.code.map_or("signal".to_string(), |c| c.to_string()),
            started.elapsed().as_secs_f64()
        );
        Ok(output)
    }

    /// Run and require exit code 0.
    pub async fn ok(self) -> Result<Output> {
        let shown = self.display();
        let out = self.output().await?;
        if !out.success() {
            return Err(eyre!(
                "`{shown}` failed (exit {:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                out.code,
                clip(&out.stdout),
                clip(&out.stderr)
            ));
        }
        Ok(out)
    }

    /// Run and require a non-zero exit code.
    pub async fn fails(self) -> Result<Output> {
        let shown = self.display();
        let out = self.output().await?;
        if out.success() {
            return Err(eyre!(
                "`{shown}` unexpectedly succeeded\n--- stdout ---\n{}\n--- stderr ---\n{}",
                clip(&out.stdout),
                clip(&out.stderr)
            ));
        }
        Ok(out)
    }

    /// Run, require success and parse stdout as JSON.
    pub async fn json(self) -> Result<Value> {
        self.ok().await?.json()
    }
}

pub fn clip(s: &str) -> String {
    const MAX: usize = 6000;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = s.len() - MAX;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &s[cut..])
}
