//! One running server + runner pair with seeded demo repositories.
//!
//! [`Stack::up`] is used by both the `#[ignore]`d E2E tests and the
//! `e2e_stack` example behind `tests/e2e/up.sh`, so the manual and automated
//! paths boot and seed servers identically.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{eyre, Context, Result};

use super::api::Api;
use super::docker;
use super::seed::{self, SeededRepo};
use super::targets::{Family, Target, JOB_IMAGE, RUNNER_LABEL};
use super::util;

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(180);
const RUNNER_READY_TIMEOUT: Duration = Duration::from_secs(120);
const LOG_TAIL_LINES: usize = 150;

#[derive(Clone, Debug)]
pub struct Account {
    pub username: String,
    pub password: String,
    pub email: String,
    /// Personal access token with scope `all`.
    pub token: String,
}

#[derive(Clone, Debug, Default)]
pub struct StackOptions {
    /// Keep containers and the work directory after the stack is dropped.
    pub keep: bool,
    /// Parent of a persistent work directory `<work_root>/<stack id>`
    /// (default: a temp dir, removed on drop unless `keep`).
    pub work_root: Option<PathBuf>,
}

pub struct Stack {
    pub target: &'static Target,
    /// Unique stack id, `fjex-e2e-<target>-<suffix>`; prefix of every container name.
    pub id: String,
    pub base_url: String,
    pub port: u16,
    pub server_version: String,
    pub server_container: String,
    pub runner_container: String,
    pub admin: Account,
    /// Second, non-admin account (reviews, approvals).
    pub reviewer: Account,
    pub org: String,
    pub repos: Vec<SeededRepo>,
    pub work_dir: PathBuf,
    pub api: Api,
    temp: Option<tempfile::TempDir>,
    keep: bool,
    healthy: bool,
}

impl Stack {
    pub async fn up(target: &'static Target, options: StackOptions) -> Result<Stack> {
        let started = Instant::now();
        let id = format!(
            "{}{}-{}",
            docker::NAME_PREFIX,
            target.id,
            util::unique_suffix()
        );
        let (work_dir, temp) = match options.work_root {
            Some(root) => {
                let dir = root.join(&id);
                fs::create_dir_all(&dir)?;
                (dir, None)
            }
            None => {
                let temp = tempfile::Builder::new()
                    .prefix(&format!("{id}-"))
                    .tempdir()
                    .wrap_err("failed to create work dir")?;
                (temp.path().to_path_buf(), Some(temp))
            }
        };

        // Construct the guard before starting anything so a failure at any later
        // step still dumps logs and removes containers.
        let mut stack = Stack {
            target,
            server_container: format!("{id}-server"),
            runner_container: format!("{id}-runner"),
            id,
            base_url: String::new(),
            port: 0,
            server_version: String::new(),
            admin: Account {
                username: "e2e".into(),
                password: "e2e-password-1234".into(),
                email: "e2e@example.invalid".into(),
                token: String::new(),
            },
            reviewer: Account {
                username: "e2e-reviewer".into(),
                password: "e2e-reviewer-password-1234".into(),
                email: "e2e-reviewer@example.invalid".into(),
                token: String::new(),
            },
            org: seed::DEMO_ORG.to_string(),
            repos: Vec::new(),
            work_dir,
            api: Api::new("http://127.0.0.1:1")?,
            temp,
            keep: options.keep,
            healthy: false,
        };

        stack.log(&format!("starting {} ({})", target.label(), target.image));
        stack.start_server().await?;
        stack.create_accounts().await?;
        stack.start_runner().await?;
        stack.log("seeding demo repositories");
        let seed_dir = stack.work_dir.join("seed");
        fs::create_dir_all(&seed_dir)?;
        stack.repos = seed::seed_demo_repos(&seed::SeedInput {
            api: &stack.api,
            base_url: &stack.base_url,
            admin_username: &stack.admin.username,
            admin_token: &stack.admin.token,
            workflow_dir: target.workflow_dir,
            stack_tag: stack.id.rsplit('-').next().unwrap_or_default(),
            work_dir: &seed_dir,
        })
        .await?;
        stack.log(&format!(
            "ready in {:.1}s: {} (server {}) with {} demo repositories",
            started.elapsed().as_secs_f64(),
            stack.base_url,
            stack.server_version,
            stack.repos.len()
        ));
        Ok(stack)
    }

    fn log(&self, msg: &str) {
        eprintln!("[{}] {msg}", self.target.id);
    }

    fn labels(&self, role: &str) -> Vec<String> {
        vec![
            "--label".into(),
            format!("{}=1", docker::LABEL),
            "--label".into(),
            format!("{}={}", docker::STACK_LABEL, self.id),
            "--label".into(),
            format!("{}={}", docker::TARGET_LABEL, self.target.id),
            "--label".into(),
            format!("fjex-e2e.role={role}"),
            "--label".into(),
            format!("{}={}", docker::WORK_DIR_LABEL, self.work_dir.display()),
        ]
    }

    async fn start_server(&mut self) -> Result<()> {
        let mut last_err = None;
        for _attempt in 0..5 {
            let port = util::pick_free_port()?;
            let base_url = format!("http://localhost:{port}");
            let mut args: Vec<String> = vec![
                "run".into(),
                "-d".into(),
                "--name".into(),
                self.server_container.clone(),
            ];
            args.extend(self.labels("server"));
            args.extend(["-p".into(), format!("127.0.0.1:{port}:3000")]);
            for env in [
                "USER_UID=1000".to_string(),
                "USER_GID=1000".to_string(),
                format!("ROOT_URL={base_url}/"),
                "DOMAIN=localhost".to_string(),
                "INSTALL_LOCK=true".to_string(),
                "GITEA__security__INSTALL_LOCK=true".to_string(),
                "GITEA__server__DISABLE_SSH=true".to_string(),
                "GITEA__actions__ENABLED=true".to_string(),
                "GITEA__service__ENABLE_BASIC_AUTHENTICATION=true".to_string(),
                "GITEA__server__DISABLE_HTTP_GIT=false".to_string(),
                "GITEA__repository__DEFAULT_BRANCH=main".to_string(),
                "GITEA__log__LEVEL=Info".to_string(),
            ] {
                args.extend(["-e".into(), env]);
            }
            args.push(self.target.image.to_string());
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            match docker::run(&argv) {
                Ok(_) => {
                    self.port = port;
                    self.base_url = base_url;
                    self.api = Api::new(&self.base_url)?;
                    return self.wait_for_server().await;
                }
                Err(e) => {
                    let msg = format!("{e:?}");
                    let _ = docker::run(&["rm", "-f", &self.server_container]);
                    if msg.contains("port is already allocated")
                        || msg.contains("address already in use")
                    {
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e.wrap_err("failed to start server container"));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| eyre!("could not find a free port")))
    }

    async fn wait_for_server(&mut self) -> Result<()> {
        let api = self.api.clone();
        let container = self.server_container.clone();
        self.server_version = util::poll(
            "the server API to answer",
            SERVER_READY_TIMEOUT,
            Duration::from_millis(500),
            || {
                let api = api.clone();
                let container = container.clone();
                async move {
                    if !docker::is_running(&container) {
                        return Err(eyre!(
                            "server container exited:\n{}",
                            docker::logs(&container, Some(80))
                        ));
                    }
                    Ok(api.version().await)
                }
            },
        )
        .await?;
        Ok(())
    }

    fn server_exec(&self, args: &[&str]) -> Result<String> {
        let mut argv = vec![
            "exec",
            "-u",
            "git",
            self.server_container.as_str(),
            self.target.family.server_bin(),
        ];
        argv.extend_from_slice(args);
        docker::run_quiet(&argv)
    }

    async fn create_accounts(&mut self) -> Result<()> {
        let admin = self.admin.clone();
        // The admin is created with the server CLI (no API user exists yet).
        let mut created = Err(eyre!("admin user was not created"));
        for _ in 0..10 {
            created = self.server_exec(&[
                "admin",
                "user",
                "create",
                "--username",
                &admin.username,
                "--password",
                &admin.password,
                "--email",
                &admin.email,
                "--admin",
                "--must-change-password=false",
            ]);
            if created.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        created.wrap_err("failed to create the admin user")?;

        self.admin.token = self
            .api
            .create_token(&admin.username, &admin.password, "fjex-e2e-admin", &["all"])
            .await?;

        let reviewer = self.reviewer.clone();
        self.api
            .create_user(
                &self.admin.token,
                &reviewer.username,
                &reviewer.password,
                &reviewer.email,
            )
            .await?;
        self.reviewer.token = self
            .api
            .create_token(
                &reviewer.username,
                &reviewer.password,
                "fjex-e2e-reviewer",
                &["all"],
            )
            .await?;
        Ok(())
    }

    fn runner_config(&self) -> String {
        format!(
            "log:\n  level: info\nrunner:\n  file: /data/.runner\n  capacity: 2\n  timeout: 20m\n  fetch_interval: 1s\n  report_interval: 1s\n  labels:\n    - \"{label}\"\ncache:\n  enabled: false\ncontainer:\n  network: host\n  force_pull: false\n  options: \"--label {l}=1 --label {sl}={id}\"\n",
            label = self.runner_label(),
            l = docker::LABEL,
            sl = docker::STACK_LABEL,
            id = self.id,
        )
    }

    fn runner_label(&self) -> String {
        format!("{RUNNER_LABEL}:docker://{JOB_IMAGE}")
    }

    async fn start_runner(&mut self) -> Result<()> {
        let output = self
            .server_exec(&["actions", "generate-runner-token"])
            .wrap_err("failed to generate a runner registration token")?;
        let token = output
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.contains(char::is_whitespace))
            .ok_or_else(|| eyre!("generate-runner-token printed no token"))?
            .to_string();

        let runner_dir = self.work_dir.join("runner");
        fs::create_dir_all(&runner_dir)?;
        let config_path = runner_dir.join("config.yml");
        fs::write(&config_path, self.runner_config())?;
        let token_path = runner_dir.join("registration-token");
        write_private(&token_path, &token)?;

        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            self.runner_container.clone(),
        ];
        args.extend(self.labels("runner"));
        args.extend([
            "--network".into(),
            "host".into(),
            "-v".into(),
            "/var/run/docker.sock:/var/run/docker.sock".into(),
        ]);
        match self.target.family {
            Family::Forgejo => {
                args.extend(["--user".into(), "0:0".into()]);
                for env in [
                    format!("FJEX_INSTANCE={}", self.base_url),
                    format!("FJEX_RUNNER_NAME={}", self.runner_container),
                    format!("FJEX_RUNNER_LABELS={}", self.runner_label()),
                ] {
                    args.extend(["-e".into(), env]);
                }
                args.push(self.target.runner_image.to_string());
                args.extend([
                    "/bin/sh".into(),
                    "-c".into(),
                    concat!(
                        "set -e; ",
                        "if [ ! -s /data/.runner ]; then ",
                        "forgejo-runner register --no-interactive --instance \"$FJEX_INSTANCE\" ",
                        "--token \"$(cat /data/registration-token)\" --name \"$FJEX_RUNNER_NAME\" ",
                        "--labels \"$FJEX_RUNNER_LABELS\" -c /data/config.yml; ",
                        "fi; ",
                        "rm -f /data/registration-token; ",
                        "exec forgejo-runner daemon -c /data/config.yml"
                    )
                    .into(),
                ]);
            }
            Family::Gitea => {
                for env in [
                    format!("GITEA_INSTANCE_URL={}", self.base_url),
                    "GITEA_RUNNER_REGISTRATION_TOKEN_FILE=/data/registration-token".to_string(),
                    format!("GITEA_RUNNER_NAME={}", self.runner_container),
                    format!("GITEA_RUNNER_LABELS={}", self.runner_label()),
                    "GITEA_MAX_REG_ATTEMPTS=10".to_string(),
                    "CONFIG_FILE=/data/config.yml".to_string(),
                ] {
                    args.extend(["-e".into(), env]);
                }
                args.push(self.target.runner_image.to_string());
            }
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        docker::run(&argv).wrap_err("failed to create runner container")?;

        // Files go in with `docker cp` rather than a bind mount, so nothing the
        // container writes as root ends up in the host work dir.
        let config_dest = format!("{}:/data/config.yml", self.runner_container);
        docker::run(&["cp", &config_path.to_string_lossy(), &config_dest])?;
        let token_dest = format!("{}:/data/registration-token", self.runner_container);
        docker::run_quiet(&["cp", &token_path.to_string_lossy(), &token_dest])?;
        let _ = fs::remove_file(&token_path);
        docker::run(&["start", &self.runner_container])?;

        let runner = self.runner_container.clone();
        util::poll(
            "the runner to register",
            RUNNER_READY_TIMEOUT,
            Duration::from_millis(500),
            || {
                let runner = runner.clone();
                async move {
                    let logs = docker::logs(&runner, None);
                    let declared = logs
                        .lines()
                        .any(|l| l.contains("eclare") && l.contains("successfully"));
                    if declared {
                        return Ok(Some(()));
                    }
                    if !docker::is_running(&runner) {
                        return Err(eyre!("runner container exited:\n{}", util::tail(&logs, 60)));
                    }
                    Ok(None)
                }
            },
        )
        .await
        .map_err(|e| {
            e.wrap_err(format!(
                "runner logs:\n{}",
                util::tail(&docker::logs(&self.runner_container, None), 40)
            ))
        })?;
        self.log("runner registered");
        Ok(())
    }

    pub fn repo(&self, name: &str) -> &SeededRepo {
        self.repos
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("demo repository {name} was not seeded"))
    }

    /// Mark the stack as healthy: no log dump on drop.
    pub fn mark_ok(&mut self) {
        self.healthy = true;
    }

    pub fn keep(&self) -> bool {
        self.keep
    }

    /// Dump server and runner logs: a tail to stderr and the full logs to
    /// `<work_dir>/logs/` (and `$FJ_EX_E2E_LOG_DIR/<stack id>/` when set).
    pub fn dump_logs(&self) {
        let mut dirs = vec![self.work_dir.join("logs")];
        if let Some(dir) = std::env::var_os("FJ_EX_E2E_LOG_DIR") {
            dirs.push(PathBuf::from(dir).join(&self.id));
        }
        let mut containers = vec![
            ("server".to_string(), self.server_container.clone()),
            ("runner".to_string(), self.runner_container.clone()),
        ];
        // Job containers that are still around (e.g. a job that hung).
        for cid in docker::containers_with_label(docker::STACK_LABEL, Some(&self.id)) {
            let name = docker::run(&["inspect", "-f", "{{.Name}}", &cid]).unwrap_or(cid.clone());
            let name = name.trim_start_matches('/').to_string();
            if name != self.server_container && name != self.runner_container {
                containers.push((format!("job-{cid}"), name));
            }
        }
        for (role, name) in &containers {
            let logs = docker::logs(name, None);
            eprintln!(
                "\n===== [{}] {role} container {name}: last {LOG_TAIL_LINES} log lines =====\n{}\n===== end {role} =====",
                self.target.id,
                util::tail(&logs, LOG_TAIL_LINES)
            );
            for dir in &dirs {
                if fs::create_dir_all(dir).is_ok() {
                    let _ = fs::write(dir.join(format!("{role}.log")), &logs);
                }
            }
        }
        for dir in &dirs {
            eprintln!(
                "[{}] full container logs written to {}",
                self.target.id,
                dir.display()
            );
        }
    }

    /// Environment-style description of the stack for manual use.
    pub fn env_file_contents(&self, appdata: &Path) -> String {
        let mut s = String::new();
        s.push_str(&format!("FJEX_E2E_STACK={}\n", self.id));
        s.push_str(&format!("FJEX_E2E_TARGET={}\n", self.target.id));
        s.push_str(&format!("FJEX_E2E_BASE_URL={}\n", self.base_url));
        s.push_str(&format!(
            "FJEX_E2E_SERVER_VERSION={}\n",
            self.server_version
        ));
        s.push_str(&format!("FJEX_E2E_ADMIN_USER={}\n", self.admin.username));
        s.push_str(&format!(
            "FJEX_E2E_ADMIN_PASSWORD={}\n",
            self.admin.password
        ));
        s.push_str(&format!("FJEX_E2E_ADMIN_TOKEN={}\n", self.admin.token));
        s.push_str(&format!(
            "FJEX_E2E_REVIEWER_USER={}\n",
            self.reviewer.username
        ));
        s.push_str(&format!(
            "FJEX_E2E_REVIEWER_PASSWORD={}\n",
            self.reviewer.password
        ));
        s.push_str(&format!("FJEX_E2E_ORG={}\n", self.org));
        s.push_str(&format!(
            "FJEX_E2E_REPOS=\"{}\"\n",
            self.repos
                .iter()
                .map(|r| r.full_name.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        s.push_str(&format!("APPDATA={}\n", appdata.display()));
        s
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let failed = !self.healthy || std::thread::panicking();
        if failed && !self.base_url.is_empty() {
            self.dump_logs();
        }
        if self.keep {
            if let Some(temp) = self.temp.take() {
                let _ = temp.keep();
            }
            eprintln!(
                "[{}] keeping stack {} running at {} (work dir {}). Remove with: tests/e2e/down.sh {}",
                self.target.id,
                self.id,
                self.base_url,
                self.work_dir.display(),
                self.id
            );
            return;
        }
        let removed = docker::remove_stack(&self.id);
        if removed > 0 {
            eprintln!(
                "[{}] removed {removed} container(s) of {}",
                self.target.id, self.id
            );
        }
    }
}

/// Write a file readable only by the current user.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
        f.write_all(contents.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents).wrap_err_with(|| format!("failed to write {}", path.display()))
    }
}

/// Write the `keys.json` token store the official `fj` CLI uses, so `fj-ex`
/// commands that need an API token (runners, token, pulls) find one.
pub fn write_fj_keys_json(appdata: &Path, base_url: &str, token: &str) -> Result<PathBuf> {
    let url = url::Url::parse(base_url).wrap_err("invalid base url")?;
    let host = url
        .host_str()
        .ok_or_else(|| eyre!("base url has no host"))?;
    let host_key = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let dir = appdata.join("Cyborus").join("forgejo-cli").join("data");
    fs::create_dir_all(&dir)?;
    let path = dir.join("keys.json");
    let json = serde_json::json!({
        "hosts": { host_key: { "type": "Application", "name": "e2e", "token": token } },
        "aliases": {},
    });
    write_private(&path, &serde_json::to_string_pretty(&json)?)?;
    Ok(path)
}
