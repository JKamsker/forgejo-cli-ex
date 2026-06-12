use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use eyre::{eyre, Context, Result};
use serde_json::Value;
use url::Url;

const FORGEJO_IMAGE: &str = "codeberg.org/forgejo/forgejo:14.0.2";
const FORGEJO_IMAGE_11_0_10: &str = "codeberg.org/forgejo/forgejo:11.0.10";
const FORGEJO_IMAGE_15_0_0: &str = "codeberg.org/forgejo/forgejo:15.0.0";
const ACT_RUNNER_IMAGE: &str = "gitea/act_runner:0.3.0";

#[tokio::test]
#[ignore]
async fn e2e_forgejo_14_0_2_docker() -> Result<()> {
    run_e2e(FORGEJO_IMAGE, "14-0-2").await
}

#[tokio::test]
#[ignore]
async fn e2e_forgejo_11_0_10_docker() -> Result<()> {
    run_e2e(FORGEJO_IMAGE_11_0_10, "11-0-10").await
}

#[tokio::test]
#[ignore]
async fn e2e_forgejo_15_0_0_docker() -> Result<()> {
    run_e2e(FORGEJO_IMAGE_15_0_0, "15-0-0").await
}

async fn run_e2e(forgejo_image: &str, version_label: &str) -> Result<()> {
    if !cfg!(target_os = "linux") {
        eprintln!("skipping e2e: requires Linux (uses Docker host networking for job containers)");
        return Ok(());
    }
    if std::env::var("FJ_EX_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping e2e: set FJ_EX_E2E=1 to enable");
        return Ok(());
    }
    if !docker_available() {
        eprintln!("skipping e2e: docker is not available");
        return Ok(());
    }

    let http_port = pick_free_port()?;
    let base_url = format!("http://localhost:{http_port}");

    let username = "e2e";
    let password = "e2e-password-1234";
    let email = "e2e@example.invalid";
    let repo_name = format!("fj-ex-e2e-{version_label}-{http_port}");
    let repo = format!("{username}/{repo_name}");

    let temp = tempfile::tempdir().wrap_err("failed to create temp dir")?;
    let appdata_dir = temp.path().join("appdata");
    let runner_data_dir = temp.path().join("runner");
    let repo_dir = temp.path().join("repo");
    let logs_dir = temp.path().join("logs");
    let smoke_dir = temp.path().join("smoke");
    let artifact_zip = temp.path().join("artifact.zip");

    fs::create_dir_all(&appdata_dir).wrap_err("failed to create appdata dir")?;
    fs::create_dir_all(&runner_data_dir).wrap_err("failed to create runner data dir")?;
    fs::create_dir_all(&repo_dir).wrap_err("failed to create git repo dir")?;
    fs::create_dir_all(&logs_dir).wrap_err("failed to create logs dir")?;
    fs::create_dir_all(&smoke_dir).wrap_err("failed to create smoke dir")?;

    let test_id = format!("{}-{version_label}-{http_port}", std::process::id());
    let forgejo_name = format!("fj-ex-e2e-forgejo-{test_id}");
    let runner_name = format!("fj-ex-e2e-runner-{test_id}");

    let _stack = DockerStack::start(
        &forgejo_name,
        &runner_name,
        &runner_data_dir,
        &base_url,
        forgejo_image,
        username,
        password,
        email,
    )
    .await?;

    api_create_repo(&base_url, username, password, &repo_name).await?;
    git_push_workflow(&repo_dir, &base_url, username, password, &repo_name).await?;

    let fj_ex = fj_ex_bin()?;

    // Runners (REST API) requires `fj`'s stored API token. Ensure missing-token UX is clear.
    let missing_token_out = fj_ex_cmd_expect_failure(
        &fj_ex,
        &appdata_dir,
        &[
            "actions", "--host", &base_url, "runners", "token", "--scope", "global",
        ],
        None,
    )?;
    assert!(
        missing_token_out
            .stderr
            .contains("No Forgejo API token found"),
        "expected missing token error, got stderr:\n{}",
        missing_token_out.stderr
    );
    assert!(
        missing_token_out.stderr.contains("fj --host"),
        "expected fj auth instructions, got stderr:\n{}",
        missing_token_out.stderr
    );
    assert!(
        missing_token_out.stderr.contains("keys.json"),
        "expected keys.json path mention, got stderr:\n{}",
        missing_token_out.stderr
    );

    // Create an API token and write it to the same keys.json store that `fj` uses.
    let fj_api_token = docker_stdout(&[
        "exec",
        "-u",
        "git",
        &forgejo_name,
        "forgejo",
        "admin",
        "user",
        "generate-access-token",
        "--username",
        username,
        "--token-name",
        "fj-ex-e2e",
        "--scopes",
        "all",
        "--raw",
    ])
    .wrap_err("failed to generate api token")?;
    if fj_api_token.trim().is_empty() {
        return Err(eyre!("api token was empty"));
    }
    write_fj_keys_json(&appdata_dir, &base_url, fj_api_token.trim())?;

    // Login (non-interactive)
    fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "auth",
            "login",
            "--host",
            &base_url,
            "--username",
            username,
            "--password-stdin",
        ],
        Some(format!("{password}\n").into_bytes()),
    )?;

    // Auth status/list/show
    let status_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &["auth", "status", "--host", &base_url, "--json"],
        None,
    )?;
    let status_json: Value = serde_json::from_str(&status_out.stdout)?;
    assert_eq!(status_json["baseUrl"], base_url);
    assert_eq!(status_json["username"], username);
    assert_eq!(status_json["hasCreds"], true);
    assert_eq!(status_json["sessionOk"], true);

    let list_out = fj_ex_cmd(&fj_ex, &appdata_dir, &["auth", "list", "--json"], None)?;
    let list_json: Value = serde_json::from_str(&list_out.stdout)?;
    assert!(list_json["logins"]
        .as_array()
        .is_some_and(|a| !a.is_empty()));

    let show_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &["auth", "show", "--host", &base_url, "--json"],
        None,
    )?;
    let show_json: Value = serde_json::from_str(&show_out.stdout)?;
    assert_eq!(show_json["username"], username);
    assert_eq!(show_json["hasPassword"], true);

    // Mint a Forgejo NuGet API key entirely through fj-ex.
    let nuget_key_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &["token", "mint", "nuget", "--host", &base_url, "--json"],
        None,
    )?;
    let nuget_key_json: Value = serde_json::from_str(&nuget_key_out.stdout)?;
    assert_eq!(nuget_key_json["baseUrl"], base_url);
    assert_eq!(nuget_key_json["owner"], username);
    assert_eq!(nuget_key_json["username"], username);
    assert_eq!(
        nuget_key_json["registryUrl"],
        format!("{base_url}/api/packages/{username}/nuget/index.json")
    );
    assert_eq!(nuget_key_json["scope"], "write:package");
    assert!(
        nuget_key_json["apiKey"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "expected nuget api key, got: {}",
        nuget_key_out.stdout
    );
    let minted_token_name = nuget_key_json["tokenName"]
        .as_str()
        .ok_or_else(|| eyre!("minted token name missing"))?;

    // Runner registration tokens (global/repo/user)
    let token_list_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &["token", "list", "--host", &base_url, "--json"],
        None,
    )?;
    let token_list_json: Value = serde_json::from_str(&token_list_out.stdout)?;
    assert_eq!(token_list_json["baseUrl"], base_url);
    assert_eq!(token_list_json["username"], username);
    assert!(
        token_list_json["tokens"].as_array().is_some_and(|tokens| {
            tokens.iter().any(|tok| {
                tok["name"].as_str() == Some(minted_token_name)
                    && tok["scopes"]
                        .as_array()
                        .is_some_and(|scopes| scopes.iter().any(|scope| scope == "write:package"))
            })
        }),
        "expected minted token in token list, got: {}",
        token_list_out.stdout
    );

    let token_repo_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions", "--host", &base_url, "--repo", &repo, "runners", "token", "--json",
        ],
        None,
    )?;
    let token_repo_json: Value = serde_json::from_str(&token_repo_out.stdout)?;
    assert_eq!(token_repo_json["baseUrl"], base_url);
    assert_eq!(token_repo_json["scope"], "repo");
    assert_eq!(token_repo_json["repo"], repo);
    assert!(
        token_repo_json["token"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "expected repo token, got: {}",
        token_repo_out.stdout
    );

    let token_global_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions", "--host", &base_url, "runners", "token", "--scope", "global", "--json",
        ],
        None,
    )?;
    let token_global_json: Value = serde_json::from_str(&token_global_out.stdout)?;
    assert_eq!(token_global_json["baseUrl"], base_url);
    assert_eq!(token_global_json["scope"], "global");
    assert!(
        token_global_json["token"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "expected global token, got: {}",
        token_global_out.stdout
    );

    let token_user_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions", "--host", &base_url, "runners", "token", "--scope", "user", "--json",
        ],
        None,
    )?;
    let token_user_json: Value = serde_json::from_str(&token_user_out.stdout)?;
    assert_eq!(token_user_json["baseUrl"], base_url);
    assert_eq!(token_user_json["scope"], "user");
    assert!(
        token_user_json["token"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "expected user token, got: {}",
        token_user_out.stdout
    );

    // Wait for the first run to exist and finish.
    let run_index = wait_for_first_run(&fj_ex, &appdata_dir, &base_url, &repo).await?;
    wait_for_run_success(&fj_ex, &appdata_dir, &base_url, &repo, run_index).await?;

    // Workflows/runs/jobs listing
    let workflows_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "workflows",
            "--json",
        ],
        None,
    )?;
    let workflows_json: Value = serde_json::from_str(&workflows_out.stdout)?;
    assert!(
        workflows_json["workflows"]
            .as_array()
            .is_some_and(|w| !w.is_empty()),
        "expected at least one workflow, got: {}",
        workflows_out.stdout
    );

    let runs_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions", "--host", &base_url, "--repo", &repo, "runs", "--limit", "5", "--json",
        ],
        None,
    )?;
    let runs_json: Value = serde_json::from_str(&runs_out.stdout)?;
    assert!(runs_json["runs"].as_array().is_some_and(|r| !r.is_empty()));

    let jobs_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "jobs",
            "--run-index",
            &run_index.to_string(),
            "--json",
        ],
        None,
    )?;
    let jobs_json: Value = serde_json::from_str(&jobs_out.stdout)?;
    assert!(jobs_json["jobs"].as_array().is_some_and(|j| !j.is_empty()));

    // Logs (job to stdout + run to files)
    let logs_job_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "logs",
            "job",
            "--run-index",
            &run_index.to_string(),
            "--job-index",
            "0",
        ],
        None,
    )?;
    assert!(
        logs_job_out.stdout.contains("fj-ex-e2e: hello"),
        "expected marker in job logs, got:\n{}",
        logs_job_out.stdout
    );

    let _logs_run_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "logs",
            "run",
            "--run-index",
            &run_index.to_string(),
            "--out-dir",
            logs_dir.to_str().unwrap_or("."),
        ],
        None,
    )?;
    assert!(
        fs::read_dir(&logs_dir).is_ok_and(|mut it| it.next().is_some()),
        "expected at least one log file in {}",
        logs_dir.display()
    );

    // Artifacts list + download (zip)
    let artifacts_list_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "artifacts",
            "list",
            "--run-index",
            &run_index.to_string(),
            "--json",
        ],
        None,
    )?;
    assert!(
        artifacts_list_out.stdout.contains("\"my-artifact\"")
            || artifacts_list_out.stdout.contains("my-artifact"),
        "expected artifact in output, got:\n{}",
        artifacts_list_out.stdout
    );

    fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "artifacts",
            "get",
            "--run-index",
            &run_index.to_string(),
            "--artifact",
            "my-artifact",
            "--out-file",
            artifact_zip.to_str().unwrap_or("artifact.zip"),
        ],
        None,
    )?;
    let artifact_bytes = fs::read(&artifact_zip).wrap_err("failed to read artifact zip")?;
    assert!(
        artifact_bytes.starts_with(b"PK"),
        "expected zip file magic bytes, got: {:?}",
        &artifact_bytes.get(..4)
    );

    // Cancel/rerun dry-run
    let cancel_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "cancel",
            "--run-index",
            &run_index.to_string(),
            "--dry-run",
        ],
        None,
    )?;
    assert!(cancel_out.stdout.contains("DRY RUN: POST"));
    assert!(cancel_out.stdout.contains("/cancel"));

    let rerun_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "rerun",
            "--run-index",
            &run_index.to_string(),
            "--dry-run",
        ],
        None,
    )?;
    assert!(rerun_out.stdout.contains("DRY RUN: POST"));
    assert!(rerun_out.stdout.contains("/rerun"));

    // Smoke test uses the same UI endpoints and downloads logs.
    let smoke_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "smoke-test",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "--out-dir",
            smoke_dir.to_str().unwrap_or("."),
        ],
        None,
    )?;
    assert!(smoke_out.stdout.lines().any(|l| l.trim() == "OK"));

    // Trigger a workflow with a mismatched runs-on label to ensure runner jobs can be listed/filtered.
    let _trigger_waiting_out = fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &[
            "actions",
            "--host",
            &base_url,
            "--repo",
            &repo,
            "trigger",
            "--workflow",
            "e2e-waiting.yml",
            "--ref",
            "main",
            "--json",
        ],
        None,
    )?;
    let waiting_jobs_json =
        wait_for_waiting_runner_jobs(&fj_ex, &appdata_dir, &base_url, &repo, "missing-label")
            .await?;
    assert_eq!(waiting_jobs_json["baseUrl"], base_url);
    assert_eq!(waiting_jobs_json["scope"], "repo");
    assert_eq!(waiting_jobs_json["repo"], repo);
    assert_eq!(waiting_jobs_json["waitingOnly"], true);
    assert!(waiting_jobs_json["jobs"]
        .as_array()
        .is_some_and(|a| !a.is_empty()));

    // Logout should remove the entry.
    fj_ex_cmd(
        &fj_ex,
        &appdata_dir,
        &["auth", "logout", "--host", &base_url],
        None,
    )?;

    Ok(())
}

struct DockerStack {
    forgejo_name: String,
    runner_name: String,
}

impl DockerStack {
    async fn start(
        forgejo_name: &str,
        runner_name: &str,
        runner_data_dir: &Path,
        base_url: &str,
        forgejo_image: &str,
        username: &str,
        password: &str,
        email: &str,
    ) -> Result<Self> {
        docker_status(&[
            "run",
            "-d",
            "--name",
            forgejo_name,
            "-p",
            &format!("{}:3000", base_url.rsplit(':').next().unwrap_or("3000")),
            "-e",
            "USER_UID=1000",
            "-e",
            "USER_GID=1000",
            "-e",
            &format!("ROOT_URL={base_url}/"),
            "-e",
            "DOMAIN=localhost",
            "-e",
            "INSTALL_LOCK=true",
            "-e",
            "SECRET_KEY=fj-ex-e2e-secret",
            "-e",
            "GITEA__actions__ENABLED=true",
            "-e",
            "GITEA__actions__DEFAULT_ACTIONS_URL=https://github.com",
            "-e",
            "GITEA__service__ENABLE_BASIC_AUTHENTICATION=true",
            "-e",
            "GITEA__server__DISABLE_HTTP_GIT=false",
            forgejo_image,
        ])
        .wrap_err("failed to start forgejo container")?;

        wait_for_http(base_url, Duration::from_secs(90))
            .await
            .wrap_err_with(|| format!("forgejo never became ready at {base_url}"))?;

        docker_status(&[
            "exec",
            "-u",
            "git",
            forgejo_name,
            "forgejo",
            "admin",
            "user",
            "create",
            "--username",
            username,
            "--password",
            password,
            "--email",
            email,
            "--admin",
            "--must-change-password=false",
        ])
        .wrap_err("failed to create forgejo admin user")?;

        let runner_token = docker_stdout(&[
            "exec",
            "-u",
            "git",
            forgejo_name,
            "forgejo",
            "actions",
            "generate-runner-token",
        ])
        .wrap_err("failed to generate runner token")?;
        let runner_token = runner_token.trim();
        if runner_token.is_empty() {
            return Err(eyre!("runner token was empty"));
        }

        let runner_config_path = runner_data_dir.join("config.yml");
        fs::write(
            &runner_config_path,
            "log:\n  level: info\ncontainer:\n  network: host\n  force_pull: false\n",
        )
        .wrap_err("failed to write runner config")?;

        docker_status(&[
            "run",
            "-d",
            "--name",
            runner_name,
            "--network",
            "host",
            "-e",
            &format!("GITEA_INSTANCE_URL={base_url}"),
            "-e",
            &format!("GITEA_RUNNER_REGISTRATION_TOKEN={runner_token}"),
            "-e",
            &format!("GITEA_RUNNER_NAME={runner_name}"),
            "-e",
            "GITEA_RUNNER_LABELS=ubuntu-latest:docker://node:20-bookworm",
            "-e",
            "CONFIG_FILE=/data/config.yml",
            "-v",
            "/var/run/docker.sock:/var/run/docker.sock",
            "-v",
            &format!("{}:/data", runner_data_dir.display()),
            ACT_RUNNER_IMAGE,
        ])
        .wrap_err("failed to start act_runner container")?;

        wait_for_runner(runner_name, Duration::from_secs(90))
            .await
            .wrap_err("runner did not register")?;

        Ok(Self {
            forgejo_name: forgejo_name.to_string(),
            runner_name: runner_name.to_string(),
        })
    }
}

impl Drop for DockerStack {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.runner_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.forgejo_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn pick_free_port() -> Result<u16> {
    // NOTE: This has a small TOCTOU race (another process could bind the port
    // after we release it and before Docker binds). In CI this is extremely
    // unlikely, and Docker will fail fast if the port is taken.
    let listener = TcpListener::bind("127.0.0.1:0").wrap_err("failed to bind to ephemeral port")?;
    let port = listener
        .local_addr()
        .wrap_err("failed to read listener addr")?
        .port();
    Ok(port)
}

fn docker_stdout(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to run docker {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(eyre!(
            "docker {} failed (exit={}):\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn docker_status(args: &[&str]) -> Result<()> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to run docker {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(eyre!(
            "docker {} failed (exit={}):\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

async fn wait_for_http(base_url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .wrap_err("failed to build http client")?;

    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(eyre!("timeout waiting for http ready at {base_url}"));
        }

        let health_url = format!("{base_url}/api/healthz");
        match client.get(&health_url).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::OK => return Ok(()),
            _ => {}
        }

        let root_url = format!("{base_url}/");
        match client.get(&root_url).send().await {
            Ok(r) if r.status().as_u16() < 500 => return Ok(()),
            _ => {}
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_runner(runner_name: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            let logs = Command::new("docker")
                .args(["logs", runner_name])
                .output()
                .ok()
                .map(|o| {
                    let mut s = String::new();
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                    s.push_str(&String::from_utf8_lossy(&o.stderr));
                    s
                })
                .unwrap_or_default();
            return Err(eyre!(
                "timeout waiting for runner to register. last logs:\n{logs}"
            ));
        }

        let logs = Command::new("docker")
            .args(["logs", runner_name])
            .output()
            .ok()
            .map(|o| {
                let mut s = String::new();
                s.push_str(&String::from_utf8_lossy(&o.stdout));
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                s
            })
            .unwrap_or_default();

        if logs.contains("SUCCESS") || logs.contains("Runner registered successfully") {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn api_create_repo(
    base_url: &str,
    username: &str,
    password: &str,
    repo_name: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .wrap_err("failed to build http client")?;

    let url = format!("{base_url}/api/v1/user/repos");
    let body = serde_json::json!({
        "name": repo_name,
        "private": false,
        "auto_init": false,
    });

    let resp = client
        .post(&url)
        .basic_auth(username, Some(password))
        .json(&body)
        .send()
        .await
        .wrap_err("failed to call create repo api")?;

    if resp.status() == reqwest::StatusCode::CREATED || resp.status() == reqwest::StatusCode::OK {
        return Ok(());
    }

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Err(eyre!("create repo failed: HTTP {} body={}", status, text))
}

async fn git_push_workflow(
    repo_dir: &Path,
    base_url: &str,
    username: &str,
    password: &str,
    repo_name: &str,
) -> Result<()> {
    let wf_dir = repo_dir.join(".forgejo").join("workflows");
    fs::create_dir_all(&wf_dir).wrap_err("failed to create workflow dir")?;
    fs::write(
        wf_dir.join("e2e.yml"),
        r#"name: fj-ex e2e
on:
  push:
  workflow_dispatch:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: Marker
        run: |
          echo "fj-ex-e2e: hello"
          echo "fj-ex-e2e: artifact" > artifact.txt
      - name: Upload artifact
        uses: actions/upload-artifact@v3
        with:
          name: my-artifact
          path: artifact.txt
"#,
    )
    .wrap_err("failed to write workflow")?;
    fs::write(
        wf_dir.join("e2e-waiting.yml"),
        r#"name: fj-ex e2e waiting
on:
  workflow_dispatch:

jobs:
  waiting:
    runs-on: missing-label
    steps:
      - run: |
          echo "fj-ex-e2e: waiting"
"#,
    )
    .wrap_err("failed to write waiting workflow")?;
    fs::write(repo_dir.join("README.md"), "# fj-ex e2e\n").wrap_err("failed to write readme")?;

    git(repo_dir, &["init", "-b", "main"]).wrap_err("git init failed")?;
    git(repo_dir, &["config", "user.name", "fj-ex-e2e"]).wrap_err("git config user.name failed")?;
    git(
        repo_dir,
        &["config", "user.email", "fj-ex-e2e@example.invalid"],
    )
    .wrap_err("git config user.email failed")?;
    git(repo_dir, &["add", "."]).wrap_err("git add failed")?;
    git(repo_dir, &["commit", "-m", "test: add workflow"]).wrap_err("git commit failed")?;

    let port = base_url
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(3000);
    let remote =
        format!("http://{username}:{password}@localhost:{port}/{username}/{repo_name}.git");
    git(repo_dir, &["remote", "add", "origin", &remote]).wrap_err("git remote add failed")?;
    git(repo_dir, &["push", "-u", "origin", "main"]).wrap_err("git push failed")?;
    Ok(())
}

fn git(repo_dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(eyre!(
            "git {} failed (exit={}):\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn fj_ex_bin() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_fj-ex") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_fj_ex") {
        return Ok(PathBuf::from(path));
    }

    // Fallback: target/debug/<bin>, next to the integration test executable.
    let exe = std::env::current_exe().wrap_err("failed to locate current test executable")?;
    let Some(debug_dir) = exe.parent().and_then(|p| p.parent()) else {
        return Err(eyre!(
            "unable to locate built fj-ex binary (could not infer target dir from {})",
            exe.display()
        ));
    };
    let bin = debug_dir.join(if cfg!(windows) { "fj-ex.exe" } else { "fj-ex" });
    if bin.is_file() {
        return Ok(bin);
    }

    Err(eyre!(
        "unable to locate built fj-ex binary. Looked for CARGO_BIN_EXE_fj-ex/CARGO_BIN_EXE_fj_ex and {}",
        bin.display()
    ))
}

struct FjOut {
    stdout: String,
    stderr: String,
}

fn fj_ex_cmd_with_expectation(
    bin: &Path,
    appdata_dir: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
    expect_success: bool,
) -> Result<FjOut> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env("APPDATA", appdata_dir)
        .env("RUST_BACKTRACE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .wrap_err_with(|| format!("failed to spawn fj-ex {}", args.join(" ")))?;

    if let Some(input) = stdin {
        use std::io::Write;
        let Some(mut s) = child.stdin.take() else {
            return Err(eyre!("failed to open fj-ex stdin pipe"));
        };
        s.write_all(&input)
            .wrap_err("failed to write to fj-ex stdin")?;
    }

    let output: Output = child
        .wait_with_output()
        .wrap_err("failed to wait for fj-ex")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if expect_success && !output.status.success() {
        return Err(eyre!(
            "fj-ex {} failed (exit={}):\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            output.status,
            stdout,
            stderr
        ));
    }
    if !expect_success && output.status.success() {
        return Err(eyre!(
            "fj-ex {} unexpectedly succeeded:\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            stdout,
            stderr
        ));
    }

    Ok(FjOut { stdout, stderr })
}

fn fj_ex_cmd(
    bin: &Path,
    appdata_dir: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
) -> Result<FjOut> {
    fj_ex_cmd_with_expectation(bin, appdata_dir, args, stdin, true)
}

fn fj_ex_cmd_expect_failure(
    bin: &Path,
    appdata_dir: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
) -> Result<FjOut> {
    fj_ex_cmd_with_expectation(bin, appdata_dir, args, stdin, false)
}

fn write_fj_keys_json(appdata_dir: &Path, base_url: &str, token: &str) -> Result<PathBuf> {
    let host_key = host_key_from_base_url(base_url)?;

    let dir = appdata_dir.join("Cyborus").join("forgejo-cli").join("data");
    fs::create_dir_all(&dir).wrap_err("failed to create keys store dir")?;
    let path = dir.join("keys.json");

    let json = serde_json::json!({
        "hosts": {
            host_key: {
                "token": token,
            }
        },
        "aliases": {},
    });

    fs::write(&path, serde_json::to_vec_pretty(&json)?).wrap_err("failed to write keys.json")?;
    Ok(path)
}

fn host_key_from_base_url(base_url: &str) -> Result<String> {
    let url = Url::parse(base_url).wrap_err("invalid base url")?;
    let host = url
        .host_str()
        .ok_or_else(|| eyre!("base url missing host"))?;
    if let Some(port) = url.port() {
        return Ok(format!("{host}:{port}"));
    }
    Ok(host.to_string())
}

async fn wait_for_first_run(
    bin: &Path,
    appdata_dir: &Path,
    base_url: &str,
    repo: &str,
) -> Result<i64> {
    let start = Instant::now();
    let timeout = Duration::from_secs(180);

    loop {
        if start.elapsed() > timeout {
            return Err(eyre!("timeout waiting for first run to appear"));
        }

        let out = fj_ex_cmd(
            bin,
            appdata_dir,
            &[
                "actions", "--host", base_url, "--repo", repo, "runs", "--limit", "5", "--json",
            ],
            None,
        );
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                eprintln!("warn: failed to list runs (will retry): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let json: Value = match serde_json::from_str(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warn: failed to parse runs json (will retry): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        if let Some(first) = json["runs"].as_array().and_then(|a| a.first()) {
            if let Some(idx) = first["runIndex"].as_i64() {
                if idx > 0 {
                    return Ok(idx);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn wait_for_waiting_runner_jobs(
    bin: &Path,
    appdata_dir: &Path,
    base_url: &str,
    repo: &str,
    label: &str,
) -> Result<Value> {
    let start = Instant::now();
    let timeout = Duration::from_secs(120);

    loop {
        if start.elapsed() > timeout {
            return Err(eyre!("timeout waiting for waiting runner jobs to appear"));
        }

        let out = fj_ex_cmd(
            bin,
            appdata_dir,
            &[
                "actions",
                "--host",
                base_url,
                "--repo",
                repo,
                "runners",
                "jobs",
                "--waiting",
                "--label",
                label,
                "--json",
            ],
            None,
        );
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                eprintln!("warn: failed to list runner jobs (will retry): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let json: Value = match serde_json::from_str(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warn: failed to parse runner jobs json (will retry): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let Some(jobs) = json["jobs"].as_array() else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        if jobs.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        return Ok(json);
    }
}

async fn wait_for_run_success(
    bin: &Path,
    appdata_dir: &Path,
    base_url: &str,
    repo: &str,
    run_index: i64,
) -> Result<()> {
    let out = fj_ex_cmd(
        bin,
        appdata_dir,
        &[
            "actions",
            "--host",
            base_url,
            "--repo",
            repo,
            "wait",
            "--run-index",
            &run_index.to_string(),
            "--timeout",
            "240s",
            "--json",
        ],
        None,
    )?;
    let json: Value =
        serde_json::from_str(&out.stdout).wrap_err("failed to parse actions wait json output")?;
    if json["status"].as_str() != Some("success") {
        return Err(eyre!(
            "run {run_index} ended with unexpected wait status: {}",
            out.stdout
        ));
    }

    Ok(())
}
