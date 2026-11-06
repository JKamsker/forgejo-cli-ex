//! System `git`, isolated from the developer's configuration.
//!
//! Credentials never appear in argv, remote URLs or `.git/config`: they are
//! passed as an `http.<base>.extraHeader` through `GIT_CONFIG_*` environment
//! variables (git >= 2.31), scoped to the server's base URL.

use std::path::Path;
use std::process::{Command, Stdio};

use eyre::{eyre, Context, Result};

pub const AUTHOR_NAME: &str = "fj-ex e2e";
pub const AUTHOR_EMAIL: &str = "e2e@example.invalid";

#[derive(Clone)]
pub struct Git {
    base_url: String,
    auth_header: Option<String>,
}

impl Git {
    /// Anonymous access (public clones).
    pub fn anonymous(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_header: None,
        }
    }

    /// HTTP Basic auth with `username` and a password or access token.
    pub fn with_basic_auth(base_url: &str, username: &str, secret: &str) -> Self {
        let encoded = super::util::base64(format!("{username}:{secret}").as_bytes());
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth_header: Some(format!("Authorization: Basic {encoded}")),
        }
    }

    /// Clone URL for `owner/name` (never contains credentials).
    pub fn repo_url(&self, full_name: &str) -> String {
        format!("{}/{full_name}.git", self.base_url)
    }

    fn command(&self, dir: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .stdin(Stdio::null())
            // Ignore user/system config (signing, push.negotiate, credential helpers, ...).
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL);
        let mut config: Vec<(String, String)> = vec![
            ("init.defaultBranch".into(), "main".into()),
            ("commit.gpgSign".into(), "false".into()),
            ("credential.helper".into(), String::new()),
        ];
        if let Some(header) = &self.auth_header {
            config.push((
                format!("http.{}/.extraHeader", self.base_url),
                header.clone(),
            ));
        }
        cmd.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (i, (k, v)) in config.into_iter().enumerate() {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), k);
            cmd.env(format!("GIT_CONFIG_VALUE_{i}"), v);
        }
        cmd
    }

    /// Run git in `dir`; returns trimmed stdout.
    pub fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self
            .command(dir)
            .args(args)
            .output()
            .wrap_err_with(|| format!("failed to spawn git {}", args.join(" ")))?;
        if !out.status.success() {
            return Err(eyre!(
                "git {} failed ({}):\n{}{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}
