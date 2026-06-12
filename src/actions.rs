use std::io::IsTerminal;
use std::num::NonZeroU64;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eyre::{eyre, Context};
use tokio::io::AsyncWriteExt;

use crate::cli::{
    ActionsArtifactsSubcommand, ActionsCommand, ActionsLogsSubcommand, ActionsRunnersSubcommand,
    ActionsSubcommand, RunnerScope,
};

static SAFE_FILENAME_COMPONENT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"[^a-zA-Z0-9._-]+"#).expect("valid regex"));

#[derive(Debug)]
struct WaitResult {
    run_index: i64,
    run_url: String,
    jobs: Vec<crate::ui_actions::JobInfo>,
}

pub async fn run(args: ActionsCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;

    match args.command {
        ActionsSubcommand::Runners { command } => {
            run_runners(command, &target).await?;
        }
        ActionsSubcommand::Trigger {
            workflow,
            git_ref,
            input,
            dry_run,
            wait,
            interval,
            timeout,
            json,
        } => {
            let repo = require_repo_owned(&target)?;
            let repo_path = repo.trim_matches('/');
            let (owner, name) = repo_path.split_once('/').ok_or_else(|| {
                eyre!(
                    "Repo should be in the format owner/name; got '{}'. Pass --repo owner/name.",
                    repo
                )
            })?;

            let workflow = workflow.trim();
            if workflow.is_empty() {
                return Err(eyre!("--workflow cannot be empty"));
            }
            if !wait {
                if interval.is_some() {
                    return Err(eyre!("--interval requires --wait"));
                }
                if timeout.is_some() {
                    return Err(eyre!("--timeout requires --wait"));
                }
            }

            let dispatch_ref = normalize_dispatch_ref(&git_ref);
            let inputs = parse_workflow_inputs(&input)?;
            let wait_timeout = parse_optional_duration(timeout.as_deref())?;
            let wait_interval = Duration::from_secs(interval.unwrap_or(2));

            let wait_session = if wait {
                Some(
                    crate::session::UiSession::from_store_with_socket(
                        &target.base_url,
                        false,
                        target.unix_socket.as_deref(),
                    )
                    .await?,
                )
            } else {
                None
            };

            let previous_latest_run = if let Some(session) = wait_session.as_ref() {
                latest_run_index_optional(session, &repo, Some(workflow)).await?
            } else {
                None
            };

            // Convert http+unix:// URLs to http://localhost for HTTP requests
            // (the Unix socket transport is configured separately via builder.unix_socket)
            let request_base = if target.base_url.starts_with("http+unix://") {
                "http://localhost"
            } else {
                target.base_url.trim_end_matches('/')
            };

            let url = format!(
                "{}/api/v1/repos/{}/{}/actions/workflows/{}/dispatches",
                request_base,
                urlencoding::encode(owner),
                urlencoding::encode(name),
                urlencoding::encode(workflow)
            );

            let mut body = serde_json::json!({
                "ref": dispatch_ref.clone(),
            });
            if !inputs.is_empty() {
                body["inputs"] = serde_json::Value::Object(inputs);
            }

            if dry_run {
                if json {
                    let payload = serde_json::json!({
                        "baseUrl": target.base_url,
                        "repo": repo,
                        "workflow": workflow,
                        "dryRun": true,
                        "url": url,
                        "body": body,
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    println!("DRY RUN: POST {url}");
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
                return Ok(());
            }

            let creds = crate::store::get_ui_creds(&target.base_url)
                .await?
                .ok_or_else(|| {
                    eyre!(
                        "No stored UI creds for '{}'. Run `fj-ex auth login` first.",
                        target.base_url
                    )
                })?;

            let mut builder = reqwest::Client::builder()
                .user_agent(concat!(
                    env!("CARGO_PKG_NAME"),
                    "/",
                    env!("CARGO_PKG_VERSION")
                ))
                .timeout(std::time::Duration::from_secs(60));

            #[cfg(unix)]
            if let Some(socket_path) = target.unix_socket.as_deref() {
                builder = builder.unix_socket(socket_path);
            }

            let client = builder.build().wrap_err("failed to build http client")?;

            let resp = client
                .post(&url)
                .basic_auth(&creds.username, Some(&creds.password))
                .json(&body)
                .send()
                .await
                .wrap_err("trigger request failed")?;

            let status = resp.status();
            if status != reqwest::StatusCode::NO_CONTENT
                && status != reqwest::StatusCode::OK
                && status != reqwest::StatusCode::CREATED
            {
                let text = resp.text().await.unwrap_or_default();
                return Err(eyre!("Trigger failed: HTTP {} body={}", status, text));
            }

            if json {
                if wait {
                    // The final wait result is printed below as a single JSON document.
                } else {
                    let payload = serde_json::json!({
                        "baseUrl": target.base_url,
                        "repo": repo,
                        "workflow": workflow,
                        "dryRun": false,
                        "url": url,
                        "body": body,
                        "triggered": true,
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
            } else {
                println!("Triggered workflow '{workflow}' on '{dispatch_ref}'.");
            }

            if wait {
                let session = wait_session
                    .as_ref()
                    .expect("wait session must exist when --wait is set");
                let wait_started_at = Instant::now();
                let run_index = wait_for_new_run_index(
                    session,
                    &repo,
                    Some(workflow),
                    previous_latest_run,
                    wait_interval,
                    wait_timeout,
                )
                .await?;
                let remaining_timeout = remaining_timeout(wait_timeout, wait_started_at)?;
                let result = wait_for_run_completion(
                    session,
                    &repo,
                    run_index,
                    None,
                    wait_interval,
                    remaining_timeout,
                )
                .await?;
                print_wait_result(
                    &target.base_url,
                    &repo,
                    &result,
                    None,
                    json,
                    Some(serde_json::json!({
                        "workflow": workflow,
                        "ref": dispatch_ref,
                        "triggered": true,
                    })),
                )?;
                if let Some(err) = wait_terminal_error(&result.jobs, run_index, None) {
                    return Err(err);
                }
            }
        }
        command => {
            let repo = require_repo_owned(&target)?;
            let session = crate::session::UiSession::from_store_with_socket(
                &target.base_url,
                false,
                target.unix_socket.as_deref(),
            )
            .await?;

            match command {
                ActionsSubcommand::Workflows { page, limit, json } => {
                    let workflows =
                        crate::ui_actions::list_workflows(&session, &repo, page, limit).await?;
                    if json {
                        let payload = serde_json::json!({
                            "baseUrl": target.base_url,
                            "repo": repo,
                            "page": page,
                            "limit": limit,
                            "workflows": workflows,
                        });
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                        return Ok(());
                    }

                    for w in workflows {
                        println!("{w}");
                    }
                }
                ActionsSubcommand::Runs {
                    workflow,
                    status,
                    latest,
                    page,
                    limit,
                    show_url,
                    header,
                    no_header,
                    json,
                } => {
                    let page = if latest { 1 } else { page };
                    let limit = if latest { 1 } else { limit };
                    let mut runs = crate::ui_actions::list_runs(
                        &session,
                        &repo,
                        workflow.as_deref(),
                        page,
                        limit,
                    )
                    .await?;

                    if let Some(filter) = status.as_deref() {
                        let filter = normalize_run_status_filter(filter)?;
                        runs.retain(|r| {
                            r.status
                                .as_deref()
                                .is_some_and(|s| normalize_run_status(s) == filter)
                        });
                    }

                    if json {
                        let payload = serde_json::json!({
                            "baseUrl": target.base_url,
                            "repo": repo,
                            "page": page,
                            "limit": limit,
                            "workflow": workflow,
                            "status": status,
                            "runs": runs.iter().map(|r| serde_json::json!({
                                "runIndex": r.run_index,
                                "url": r.url,
                                "status": r.status.clone(),
                                "branch": r.branch.clone(),
                                "createdAt": r.created_at.clone(),
                            })).collect::<Vec<_>>(),
                        });
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                        return Ok(());
                    }

                    let show_header = crate::output::should_print_header(header, no_header);
                    let mut headers = vec!["RunIndex", "Status", "Branch", "CreatedAt"];
                    if show_url {
                        headers.push("Url");
                    }

                    let mut rows = Vec::with_capacity(runs.len());
                    for r in runs {
                        let status = r.status.as_deref().unwrap_or("?");
                        let branch = r.branch.as_deref().unwrap_or("");
                        let created_at = r.created_at.as_deref().unwrap_or("");
                        let mut row = vec![
                            r.run_index.to_string(),
                            status.to_string(),
                            branch.to_string(),
                            created_at.to_string(),
                        ];
                        if show_url {
                            row.push(r.url);
                        }
                        rows.push(row);
                    }

                    crate::output::print_table(&headers, &rows, show_header);
                }
                ActionsSubcommand::Jobs {
                    run_index,
                    latest,
                    workflow,
                    watch,
                    watch_interval,
                    header,
                    no_header,
                    json,
                } => {
                    let run_index =
                        resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
                            .await?;

                    if watch && watch_interval == 0 {
                        return Err(eyre!("--watch-interval must be >= 1"));
                    }

                    let interactive = std::io::stdout().is_terminal();
                    let mut last_sig: Option<String> = None;

                    loop {
                        let view = crate::ui_actions::get_run_view_data(&session, &repo, run_index)
                            .await
                            .wrap_err("failed to load run view")?;
                        let jobs = crate::ui_actions::get_run_jobs(run_index, &view.view)?;

                        let sig = jobs
                            .iter()
                            .map(|j| {
                                let s = j.status.as_deref().unwrap_or("?");
                                format!("{}:{}", j.job_index, s)
                            })
                            .collect::<Vec<_>>()
                            .join("|");
                        let changed = last_sig.as_deref() != Some(sig.as_str());
                        last_sig = Some(sig);

                        let done = is_run_done_from_jobs(&jobs);

                        // For watch, only print intermediate updates when interactive. Always print final.
                        let should_print = !watch || done || (interactive && changed);

                        if should_print {
                            if interactive && watch && !json {
                                use std::io::Write;
                                print!("\x1B[2J\x1B[H");
                                let _ = std::io::stdout().flush();
                            }

                            if json {
                                // In watch+json mode, only print the final JSON snapshot.
                                if !watch || done {
                                    let payload = serde_json::json!({
                                        "baseUrl": target.base_url,
                                        "repo": repo,
                                        "runIndex": run_index,
                                        "jobs": jobs.iter().map(|j| serde_json::json!({
                                            "runIndex": j.run_index,
                                            "jobIndex": j.job_index,
                                            "id": j.id,
                                            "name": j.name,
                                            "status": j.status,
                                            "canRerun": j.can_rerun,
                                            "duration": j.duration,
                                        })).collect::<Vec<_>>(),
                                    });
                                    println!("{}", serde_json::to_string_pretty(&payload)?);
                                }
                            } else {
                                let show_header =
                                    crate::output::should_print_header(header, no_header);
                                let headers = ["JobIndex", "Status", "Name"];
                                let mut rows = Vec::with_capacity(jobs.len());
                                for j in &jobs {
                                    let status = j.status.as_deref().unwrap_or("?");
                                    let name = j.name.as_deref().unwrap_or("");
                                    rows.push(vec![
                                        j.job_index.to_string(),
                                        status.to_string(),
                                        name.to_string(),
                                    ]);
                                }
                                crate::output::print_table(&headers, &rows, show_header);
                            }
                        }

                        if !watch {
                            break;
                        }

                        if done {
                            if let Some(err) = run_terminal_error_from_jobs(run_index, &jobs) {
                                return Err(err);
                            }
                            return Ok(());
                        }

                        if interactive {
                            use std::io::Write;
                            let _ = std::io::stdout().flush();
                        }

                        tokio::time::sleep(std::time::Duration::from_secs(watch_interval)).await;
                    }
                }
                ActionsSubcommand::Wait {
                    run_index,
                    latest,
                    workflow,
                    job_index,
                    interval,
                    timeout,
                    json,
                } => {
                    let run_index =
                        resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
                            .await?;
                    let timeout = parse_optional_duration(timeout.as_deref())?;
                    let interval = Duration::from_secs(interval);
                    let job_index = job_index.map(i64::from);

                    let result = wait_for_run_completion(
                        &session, &repo, run_index, job_index, interval, timeout,
                    )
                    .await?;
                    print_wait_result(&target.base_url, &repo, &result, job_index, json, None)?;
                    if let Some(err) = wait_terminal_error(&result.jobs, run_index, job_index) {
                        return Err(err);
                    }
                }
                ActionsSubcommand::Logs { command } => match command {
                    ActionsLogsSubcommand::Job {
                        run_index,
                        latest,
                        workflow,
                        job_index,
                        attempt,
                        out_file,
                    } => {
                        let run_index = resolve_run_index(
                            &session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;
                        let job_index_i64 = job_index as i64;
                        let attempt = match attempt {
                            Some(a) => a.get() as i64,
                            None => {
                                crate::ui_actions::get_job_view_meta(
                                    &session,
                                    &repo,
                                    run_index,
                                    job_index_i64,
                                )
                                .await?
                                .attempt_number
                            }
                        };
                        let resp = crate::ui_actions::open_job_logs(
                            &session,
                            &repo,
                            run_index,
                            job_index_i64,
                            attempt,
                        )
                        .await?;

                        if let Some(out_file) = out_file {
                            if let Some(parent) = out_file.parent() {
                                tokio::fs::create_dir_all(parent).await?;
                            }
                            let mut file = tokio::fs::File::create(&out_file).await?;
                            crate::ui_actions::copy_response_body(resp, &mut file).await?;
                            file.flush().await?;
                            println!("{}", out_file.display());
                        } else {
                            let mut stdout = tokio::io::stdout();
                            crate::ui_actions::copy_response_body(resp, &mut stdout).await?;
                            stdout.flush().await?;
                        }
                    }
                    ActionsLogsSubcommand::Run {
                        run_index,
                        latest,
                        workflow,
                        out_dir,
                        max_jobs,
                    } => {
                        let run_index = resolve_run_index(
                            &session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;

                        let view = crate::ui_actions::get_run_view_data(&session, &repo, run_index)
                            .await
                            .wrap_err("failed to load run view")?;
                        let mut jobs = crate::ui_actions::get_run_jobs(run_index, &view.view)?;

                        if let Some(max_jobs) = max_jobs {
                            if max_jobs == 0 {
                                return Err(eyre!("--max-jobs must be >= 1"));
                            }
                            if (jobs.len() as u32) > max_jobs {
                                jobs.truncate(max_jobs as usize);
                            }
                        }

                        if let Some(out_dir) = out_dir {
                            tokio::fs::create_dir_all(&out_dir).await?;
                            let mut failures: Vec<String> = Vec::new();

                            for job in &jobs {
                                let job_index = job.job_index;
                                let job_name = job.name.as_deref().unwrap_or("");
                                let safe_name = safe_filename_component(job_name, job_index);
                                let out_file =
                                    out_dir.join(format!("job-{job_index}-{safe_name}.log"));

                                let attempt = match crate::ui_actions::get_job_view_meta(
                                    &session, &repo, run_index, job_index,
                                )
                                .await
                                {
                                    Ok(m) => m.attempt_number,
                                    Err(e) => {
                                        let msg = format!("Job {job_index} ({job_name}): {e}");
                                        eprintln!("warn: {msg}");
                                        failures.push(msg);
                                        continue;
                                    }
                                };
                                match crate::ui_actions::open_job_logs(
                                    &session, &repo, run_index, job_index, attempt,
                                )
                                .await
                                {
                                    Ok(resp) => {
                                        let mut file = tokio::fs::File::create(&out_file).await?;
                                        crate::ui_actions::copy_response_body(resp, &mut file)
                                            .await?;
                                        file.flush().await?;
                                        println!(
                                            "Saved: job {} (attempt {}) -> {}",
                                            job_index,
                                            attempt,
                                            out_file.display()
                                        );
                                    }
                                    Err(e) => {
                                        let msg = format!("Job {job_index} ({job_name}): {e}");
                                        eprintln!("warn: {msg}");
                                        failures.push(msg);
                                    }
                                };
                            }

                            if !failures.is_empty() {
                                return Err(eyre!(
                                    "Some jobs failed:\n - {}",
                                    failures.join("\n - ")
                                ));
                            }

                            return Ok(());
                        }

                        // stdout mode
                        for job in &jobs {
                            let job_index = job.job_index;
                            let job_name = job.name.as_deref().unwrap_or("");

                            let attempt = crate::ui_actions::get_job_view_meta(
                                &session, &repo, run_index, job_index,
                            )
                            .await?
                            .attempt_number;

                            eprintln!(
                                "== job {} (attempt {}) :: {} ==",
                                job_index, attempt, job_name
                            );
                            let resp = crate::ui_actions::open_job_logs(
                                &session, &repo, run_index, job_index, attempt,
                            )
                            .await?;

                            let mut stdout = tokio::io::stdout();
                            crate::ui_actions::copy_response_body(resp, &mut stdout).await?;
                            stdout.flush().await?;

                            eprintln!("== end job {} ==", job_index);
                        }
                    }
                },
                ActionsSubcommand::Artifacts { command } => match command {
                    ActionsArtifactsSubcommand::List {
                        run_index,
                        latest,
                        workflow,
                        header,
                        no_header,
                        json,
                    } => {
                        let run_index = resolve_run_index(
                            &session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;

                        let artifacts =
                            crate::ui_actions::get_run_artifacts(&session, &repo, run_index)
                                .await?;

                        if json {
                            let payload = serde_json::json!({
                                "baseUrl": target.base_url,
                                "repo": repo,
                                "runIndex": run_index,
                                "artifacts": artifacts,
                            });
                            println!("{}", serde_json::to_string_pretty(&payload)?);
                            return Ok(());
                        }

                        let items = artifacts
                            .get("artifacts")
                            .and_then(|v| v.as_array())
                            .map(Vec::as_slice)
                            .unwrap_or(&[]);

                        let show_header = crate::output::should_print_header(header, no_header);
                        let headers = ["Id", "Name", "Size"];
                        let mut rows = Vec::with_capacity(items.len());
                        for a in items {
                            let id = a
                                .get("id")
                                .and_then(|v| v.as_i64())
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "?".to_string());
                            let name = a
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string();
                            let size = a
                                .get("size_in_bytes")
                                .and_then(|v| v.as_i64())
                                .or_else(|| a.get("sizeInBytes").and_then(|v| v.as_i64()))
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "?".to_string());
                            rows.push(vec![id, name, size]);
                        }
                        crate::output::print_table(&headers, &rows, show_header);
                    }
                    ActionsArtifactsSubcommand::Get {
                        run_index,
                        latest,
                        workflow,
                        artifact,
                        out_file,
                    } => {
                        let run_index = resolve_run_index(
                            &session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;
                        let resp =
                            crate::ui_actions::open_artifact(&session, &repo, run_index, &artifact)
                                .await?;
                        if let Some(parent) = out_file.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        let mut file = tokio::fs::File::create(&out_file).await?;
                        crate::ui_actions::copy_response_body(resp, &mut file).await?;
                        file.flush().await?;
                        println!("{}", out_file.display());
                    }
                },
                ActionsSubcommand::SmokeTest(args) => {
                    let cmd = crate::cli::SmokeTestCommand {
                        target: crate::cli::TargetArgs {
                            host: Some(target.base_url.clone()),
                            repo: Some(
                                repo.parse::<crate::target::RepoArg>()
                                    .map_err(|e| eyre!(e))?,
                            ),
                            remote: None,
                        },
                        opts: args,
                    };
                    crate::smoke_test::run(cmd).await?;
                }
                ActionsSubcommand::Cancel {
                    run_index,
                    latest,
                    workflow,
                    dry_run,
                    json,
                } => {
                    let run_index =
                        resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
                            .await?;
                    let repo_path = repo.trim_matches('/');
                    let url = format!(
                        "{}/{repo_path}/actions/runs/{run_index}/cancel",
                        session.base_url()
                    );

                    if dry_run {
                        if json {
                            let payload = serde_json::json!({
                                "baseUrl": target.base_url,
                                "repo": repo,
                                "runIndex": run_index,
                                "dryRun": true,
                                "url": url,
                            });
                            println!("{}", serde_json::to_string_pretty(&payload)?);
                        } else {
                            println!("DRY RUN: POST {url}");
                        }
                        return Ok(());
                    }

                    let body = serde_json::json!({});
                    let resp = session.post_json_response(&url, &body, true).await?;
                    if resp.status() != reqwest::StatusCode::OK {
                        return Err(eyre!(
                            "Failed to cancel run '{}' via '{}'. HTTP {}.",
                            run_index,
                            url,
                            resp.status()
                        ));
                    }

                    if json {
                        let payload = serde_json::json!({
                            "baseUrl": target.base_url,
                            "repo": repo,
                            "runIndex": run_index,
                            "dryRun": false,
                            "url": url,
                            "canceled": true,
                        });
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                    } else {
                        println!("Canceled run #{run_index}");
                    }
                }
                ActionsSubcommand::Rerun {
                    run_index,
                    latest,
                    workflow,
                    job_index,
                    failed_only,
                    dry_run,
                    json,
                } => {
                    let run_index =
                        resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
                            .await?;
                    let repo_path = repo.trim_matches('/');

                    if failed_only {
                        return rerun_failed_jobs(
                            &session,
                            target.base_url.as_str(),
                            &repo,
                            run_index,
                            dry_run,
                            json,
                        )
                        .await;
                    }

                    let url = match job_index {
                        Some(job_index) => format!(
                            "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/rerun",
                            session.base_url()
                        ),
                        None => format!(
                            "{}/{repo_path}/actions/runs/{run_index}/rerun",
                            session.base_url()
                        ),
                    };

                    if dry_run {
                        if json {
                            let payload = serde_json::json!({
                                "baseUrl": target.base_url,
                                "repo": repo,
                                "runIndex": run_index,
                                "jobIndex": job_index,
                                "dryRun": true,
                                "url": url,
                            });
                            println!("{}", serde_json::to_string_pretty(&payload)?);
                        } else {
                            println!("DRY RUN: POST {url}");
                        }
                        return Ok(());
                    }

                    let body = serde_json::json!({});
                    let resp = session.post_json_response(&url, &body, true).await?;
                    if resp.status() != reqwest::StatusCode::OK {
                        return Err(eyre!(
                            "Failed to rerun via '{}'. HTTP {}.",
                            url,
                            resp.status()
                        ));
                    }

                    let text = resp.text().await.unwrap_or_default();
                    let mut redirect: Option<String> = None;
                    if !text.trim().is_empty() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(redirect_url) = v.get("redirect").and_then(|v| v.as_str()) {
                                redirect = Some(redirect_url.to_string());
                            }
                        }
                    }

                    if json {
                        let payload = serde_json::json!({
                            "baseUrl": target.base_url,
                            "repo": repo,
                            "runIndex": run_index,
                            "jobIndex": job_index,
                            "dryRun": false,
                            "url": url,
                            "redirect": redirect,
                            "requested": true,
                        });
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                        return Ok(());
                    }

                    match job_index {
                        Some(job_index) => {
                            println!("Rerun requested for run #{run_index}, job #{job_index}.")
                        }
                        None => println!("Rerun requested for run #{run_index}."),
                    }
                    if let Some(redirect) = redirect {
                        println!("{redirect}");
                    }
                }
                ActionsSubcommand::Trigger { .. } => {
                    unreachable!("Trigger is handled before UiSession initialization")
                }
                ActionsSubcommand::Runners { .. } => {
                    unreachable!("Runners is handled before UiSession initialization")
                }
            }
        }
    }

    Ok(())
}

async fn latest_run_index_optional(
    session: &crate::session::UiSession,
    repo: &str,
    workflow: Option<&str>,
) -> eyre::Result<Option<i64>> {
    let runs = crate::ui_actions::list_runs(session, repo, workflow, 1, 1).await?;
    Ok(runs.first().map(|run| run.run_index))
}

async fn wait_for_new_run_index(
    session: &crate::session::UiSession,
    repo: &str,
    workflow: Option<&str>,
    previous_latest_run: Option<i64>,
    interval: Duration,
    timeout: Option<Duration>,
) -> eyre::Result<i64> {
    let started_at = Instant::now();

    loop {
        let runs = crate::ui_actions::list_runs(session, repo, workflow, 1, 20).await?;
        if let Some(run) = runs
            .iter()
            .filter(|run| previous_latest_run.map_or(true, |previous| run.run_index > previous))
            .max_by_key(|run| run.run_index)
        {
            return Ok(run.run_index);
        }

        if has_timed_out(started_at, timeout) {
            let workflow_msg = workflow
                .map(|workflow| format!(" for workflow '{workflow}'"))
                .unwrap_or_default();
            return Err(eyre!(
                "timeout waiting for a new action run{workflow_msg} to appear"
            ));
        }

        tokio::time::sleep(interval).await;
    }
}

async fn wait_for_run_completion(
    session: &crate::session::UiSession,
    repo: &str,
    run_index: i64,
    job_index: Option<i64>,
    interval: Duration,
    timeout: Option<Duration>,
) -> eyre::Result<WaitResult> {
    let started_at = Instant::now();

    loop {
        let view = crate::ui_actions::get_run_view_data(session, repo, run_index)
            .await
            .wrap_err("failed to load run view")?;
        let jobs = crate::ui_actions::get_run_jobs(run_index, &view.view)?;

        if is_wait_target_done(&jobs, job_index) {
            let repo_path = repo.trim_matches('/');
            return Ok(WaitResult {
                run_index,
                run_url: format!(
                    "{}/{repo_path}/actions/runs/{run_index}",
                    session.base_url()
                ),
                jobs,
            });
        }

        if has_timed_out(started_at, timeout) {
            let target = job_index
                .map(|job_index| format!("run {run_index} job {job_index}"))
                .unwrap_or_else(|| format!("run {run_index}"));
            return Err(eyre!("timeout waiting for {target} to complete"));
        }

        tokio::time::sleep(interval).await;
    }
}

fn print_wait_result(
    target_base_url: &str,
    repo: &str,
    result: &WaitResult,
    job_index: Option<i64>,
    json: bool,
    trigger: Option<serde_json::Value>,
) -> eyre::Result<()> {
    let status = wait_target_status(&result.jobs, job_index);

    if json {
        let mut payload = serde_json::json!({
            "baseUrl": target_base_url,
            "repo": repo,
            "runIndex": result.run_index,
            "runUrl": result.run_url,
            "jobIndex": job_index,
            "status": status,
            "jobs": result.jobs.iter().map(|job| serde_json::json!({
                "runIndex": job.run_index,
                "jobIndex": job.job_index,
                "id": job.id,
                "name": job.name,
                "status": job.status,
                "canRerun": job.can_rerun,
                "duration": job.duration,
            })).collect::<Vec<_>>(),
        });
        if let Some(trigger) = trigger {
            payload["trigger"] = trigger;
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if let Some(job_index) = job_index {
        println!(
            "Run #{} job #{} completed with status '{}'.",
            result.run_index, job_index, status
        );
    } else {
        println!(
            "Run #{} completed with status '{}'.",
            result.run_index, status
        );
    }
    println!("{}", result.run_url);

    Ok(())
}

fn is_wait_target_done(jobs: &[crate::ui_actions::JobInfo], job_index: Option<i64>) -> bool {
    if jobs.is_empty() {
        return false;
    }

    if let Some(job_index) = job_index {
        if let Some(job) = jobs.iter().find(|job| job.job_index == job_index) {
            return is_job_done(job);
        }
        return is_run_done_from_jobs(jobs);
    }

    is_run_done_from_jobs(jobs)
}

fn is_job_done(job: &crate::ui_actions::JobInfo) -> bool {
    let in_progress = ["running", "queued", "pending", "waiting"];
    job.status.as_deref().is_some_and(|status| {
        !in_progress
            .iter()
            .any(|in_progress| status.eq_ignore_ascii_case(in_progress))
    })
}

fn wait_target_status(jobs: &[crate::ui_actions::JobInfo], job_index: Option<i64>) -> String {
    if let Some(job_index) = job_index {
        return jobs
            .iter()
            .find(|job| job.job_index == job_index)
            .and_then(|job| job.status.clone())
            .unwrap_or_else(|| "missing".to_string());
    }

    let failing = ["failure", "canceled", "cancelled", "blocked"];
    for job in jobs {
        let Some(status) = job.status.as_deref() else {
            return "unknown".to_string();
        };
        if failing
            .iter()
            .any(|failing| status.eq_ignore_ascii_case(failing))
        {
            return normalize_run_status(status);
        }
    }

    "success".to_string()
}

fn wait_terminal_error(
    jobs: &[crate::ui_actions::JobInfo],
    run_index: i64,
    job_index: Option<i64>,
) -> Option<eyre::Report> {
    if let Some(job_index) = job_index {
        if jobs.is_empty() {
            return Some(eyre!("run {run_index} returned no jobs"));
        }

        let Some(job) = jobs.iter().find(|job| job.job_index == job_index) else {
            return Some(eyre!("run {run_index} completed without job {job_index}"));
        };

        let failing = ["failure", "canceled", "cancelled", "blocked"];
        if job.status.as_deref().is_some_and(|status| {
            failing
                .iter()
                .any(|failing| status.eq_ignore_ascii_case(failing))
        }) {
            let status = job.status.as_deref().unwrap_or("unknown");
            return Some(eyre!(
                "run {run_index} job {job_index} finished with status {status}"
            ));
        }

        return None;
    }

    run_terminal_error_from_jobs(run_index, jobs)
}

fn has_timed_out(started_at: Instant, timeout: Option<Duration>) -> bool {
    timeout.is_some_and(|timeout| started_at.elapsed() >= timeout)
}

fn remaining_timeout(
    timeout: Option<Duration>,
    started_at: Instant,
) -> eyre::Result<Option<Duration>> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    let elapsed = started_at.elapsed();
    if elapsed >= timeout {
        return Err(eyre!(
            "timeout waiting for triggered action run to complete"
        ));
    }
    Ok(Some(timeout - elapsed))
}

async fn rerun_failed_jobs(
    session: &crate::session::UiSession,
    target_base_url: &str,
    repo: &str,
    run_index: i64,
    dry_run: bool,
    json: bool,
) -> eyre::Result<()> {
    let repo_path = repo.trim_matches('/');

    let view = crate::ui_actions::get_run_view_data(session, repo, run_index)
        .await
        .wrap_err("failed to load run view")?;
    let jobs = crate::ui_actions::get_run_jobs(run_index, &view.view)?;
    let failed_job_indexes: Vec<i64> = jobs
        .iter()
        .filter(|j| {
            j.status
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("failure"))
        })
        .map(|j| j.job_index)
        .collect();

    if dry_run {
        if json {
            let payload = serde_json::json!({
                "baseUrl": target_base_url,
                "repo": repo,
                "runIndex": run_index,
                "failedOnly": true,
                "jobIndexes": failed_job_indexes,
                "dryRun": true,
                "requested": !failed_job_indexes.is_empty(),
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else if failed_job_indexes.is_empty() {
            println!("No failed jobs found for run #{run_index}.");
        } else {
            for job_index in &failed_job_indexes {
                let url = format!(
                    "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/rerun",
                    session.base_url()
                );
                println!("DRY RUN: POST {url}");
            }
        }
        return Ok(());
    }

    if failed_job_indexes.is_empty() {
        if json {
            let payload = serde_json::json!({
                "baseUrl": target_base_url,
                "repo": repo,
                "runIndex": run_index,
                "failedOnly": true,
                "jobIndexes": [],
                "dryRun": false,
                "requested": false,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("No failed jobs found for run #{run_index}.");
        }
        return Ok(());
    }

    let mut failures: Vec<String> = Vec::new();
    let mut redirects: Vec<String> = Vec::new();

    for job_index in &failed_job_indexes {
        let url = format!(
            "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/rerun",
            session.base_url()
        );
        let body = serde_json::json!({});
        let resp = match session.post_json_response(&url, &body, true).await {
            Ok(resp) => resp,
            Err(e) => {
                failures.push(format!(
                    "job {}: POST {} -> transport error: {}",
                    job_index, url, e
                ));
                continue;
            }
        };
        if resp.status() != reqwest::StatusCode::OK {
            failures.push(format!(
                "job {}: POST {} -> HTTP {}",
                job_index,
                url,
                resp.status()
            ));
            continue;
        }

        let text = resp.text().await.unwrap_or_default();
        if !text.trim().is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(redirect_url) = v.get("redirect").and_then(|v| v.as_str()) {
                    redirects.push(redirect_url.to_string());
                }
            }
        }
    }

    if !failures.is_empty() {
        if redirects.is_empty() {
            return Err(eyre!(
                "Some job reruns failed:\n - {}",
                failures.join("\n - ")
            ));
        }
        return Err(eyre!(
            "Some job reruns failed:\n - {}\nSuccessful rerun redirects:\n - {}",
            failures.join("\n - "),
            redirects.join("\n - ")
        ));
    }

    if json {
        let payload = serde_json::json!({
            "baseUrl": target_base_url,
            "repo": repo,
            "runIndex": run_index,
            "failedOnly": true,
            "jobIndexes": failed_job_indexes,
            "dryRun": false,
            "redirects": redirects,
            "requested": true,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Rerun requested for run #{run_index} (failed jobs: {}).",
            failed_job_indexes
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        for redirect in redirects {
            println!("{redirect}");
        }
    }

    Ok(())
}

fn require_repo_owned(target: &crate::target::ResolvedTarget) -> eyre::Result<String> {
    target.repo.clone().ok_or_else(|| {
        eyre!(
            "Repo could not be resolved. Pass --repo owner/name or run inside a git repo with a Forgejo remote."
        )
    })
}

fn runner_scope_name(scope: RunnerScope) -> &'static str {
    match scope {
        RunnerScope::Global => "global",
        RunnerScope::Org => "org",
        RunnerScope::Repo => "repo",
        RunnerScope::User => "user",
    }
}

fn resolve_runner_scope(
    scope: Option<RunnerScope>,
    org: Option<&str>,
    target: &crate::target::ResolvedTarget,
) -> RunnerScope {
    if let Some(scope) = scope {
        return scope;
    }
    if org.is_some() {
        return RunnerScope::Org;
    }
    if target.repo.is_some() {
        return RunnerScope::Repo;
    }
    RunnerScope::Global
}

pub(crate) fn fj_missing_api_token_error(base_url: &str) -> eyre::Report {
    let host_key = crate::target::normalize_host_key(base_url).unwrap_or_else(|_| base_url.into());
    let path = crate::store::keys_store_paths()
        .map(|p| p.path.display().to_string())
        .unwrap_or_else(|_| "%APPDATA%/Cyborus/forgejo-cli/data/keys.json".to_string());

    eyre!(
        "No Forgejo API token found for host '{host_key}'. Authenticate via `fj`:\n  fj --host {host_key} auth login\n  fj --host {host_key} auth add-key <USER>  # reads token from stdin if omitted\nToken store:\n  %APPDATA%/Cyborus/forgejo-cli/data/keys.json (Windows)\n  ~/.local/share/Cyborus/forgejo-cli/data/keys.json (Linux)\nReading: {path}"
    )
}

fn resolve_runner_endpoint_url(
    client: &crate::api::ApiClient,
    scope: RunnerScope,
    org: Option<String>,
    target: &crate::target::ResolvedTarget,
    endpoint: &str,
) -> eyre::Result<(Option<String>, Option<String>, String)> {
    match scope {
        RunnerScope::Global => Ok((
            None,
            None,
            client.api_v1_url(&format!("/admin/runners/{endpoint}")),
        )),
        RunnerScope::Org => {
            let org = org.ok_or_else(|| eyre!("--org is required for --scope org"))?;
            let url = client.api_v1_url(&format!(
                "/orgs/{}/actions/runners/{endpoint}",
                urlencoding::encode(&org)
            ));
            Ok((Some(org), None, url))
        }
        RunnerScope::Repo => {
            let owner = target.owner.as_deref().ok_or_else(|| {
                eyre!(
                    "Repo could not be resolved. Pass --repo owner/name or run inside a git repo with a Forgejo remote."
                )
            })?;
            let name = target
                .name
                .as_deref()
                .ok_or_else(|| eyre!("resolved repo is missing name"))?;
            let repo = target
                .repo
                .clone()
                .unwrap_or_else(|| format!("{owner}/{name}"));
            let url = client.api_v1_url(&format!(
                "/repos/{}/{}/actions/runners/{endpoint}",
                urlencoding::encode(owner),
                urlencoding::encode(name)
            ));
            Ok((None, Some(repo), url))
        }
        RunnerScope::User => Ok((
            None,
            None,
            client.api_v1_url(&format!("/user/actions/runners/{endpoint}")),
        )),
    }
}

async fn run_runners(
    command: ActionsRunnersSubcommand,
    target: &crate::target::ResolvedTarget,
) -> eyre::Result<()> {
    let token = crate::store::get_fj_api_token_for_base_url(&target.base_url)?
        .ok_or_else(|| fj_missing_api_token_error(&target.base_url))?;
    let client = crate::api::ApiClient::new_with_socket(
        &target.base_url,
        &token,
        target.unix_socket.as_deref(),
    )?;

    match command {
        ActionsRunnersSubcommand::Token { scope, org, json } => {
            let scope = resolve_runner_scope(scope, org.as_deref(), target);
            let (org_out, repo_out, url) =
                resolve_runner_endpoint_url(&client, scope, org, target, "registration-token")?;

            let reg: crate::api::RegistrationToken = client.get_json(&url).await?;

            if json {
                let payload = serde_json::json!({
                    "baseUrl": target.base_url,
                    "scope": runner_scope_name(scope),
                    "org": org_out,
                    "repo": repo_out,
                    "token": reg.token,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("{}", reg.token);
            }
        }
        ActionsRunnersSubcommand::Jobs {
            scope,
            org,
            label,
            waiting,
            header,
            no_header,
            json,
        } => {
            let scope = resolve_runner_scope(scope, org.as_deref(), target);

            let labels = {
                let mut out = Vec::with_capacity(label.len());
                for l in label {
                    let trimmed = l.trim();
                    if trimmed.is_empty() {
                        return Err(eyre!("--label cannot be empty"));
                    }
                    out.push(trimmed.to_string());
                }
                out
            };
            let labels_query = if labels.is_empty() {
                String::new()
            } else {
                format!(
                    "?labels={}",
                    labels
                        .iter()
                        .map(|l| urlencoding::encode(l).into_owned())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };

            let (org_out, repo_out, base_jobs_url) =
                resolve_runner_endpoint_url(&client, scope, org, target, "jobs")?;

            let url = format!("{base_jobs_url}{labels_query}");
            let mut jobs: Vec<crate::api::ActionRunJob> = client.get_json(&url).await?;

            if waiting {
                jobs.retain(|j| {
                    j.status
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case("waiting"))
                });
            }

            if json {
                let payload = serde_json::json!({
                    "baseUrl": target.base_url,
                    "scope": runner_scope_name(scope),
                    "org": org_out,
                    "repo": repo_out,
                    "labels": labels,
                    "waitingOnly": waiting,
                    "jobs": jobs,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }

            let show_header = crate::output::should_print_header(header, no_header);
            let headers = vec!["Id", "Status", "Name", "RunsOn"];
            let mut rows = Vec::with_capacity(jobs.len());
            for j in jobs {
                let runs_on = j.runs_on_display();
                rows.push(vec![
                    j.id.to_string(),
                    j.status.unwrap_or_else(|| "?".to_string()),
                    j.name,
                    runs_on,
                ]);
            }
            crate::output::print_table(&headers, &rows, show_header);
        }
    }

    Ok(())
}

async fn resolve_run_index(
    session: &crate::session::UiSession,
    repo: &str,
    run_index: Option<NonZeroU64>,
    latest: bool,
    workflow: Option<&str>,
) -> eyre::Result<i64> {
    if run_index.is_some() && workflow.is_some() {
        return Err(eyre!("--workflow cannot be used with --run-index"));
    }

    if let Some(n) = run_index {
        let n = n.get();
        if n > i64::MAX as u64 {
            return Err(eyre!("--run-index is too large"));
        }
        return Ok(n as i64);
    }

    if latest {
        return crate::ui_actions::latest_run_index(session, repo, workflow).await;
    }

    Err(eyre!("Pass --run-index or --latest"))
}

fn normalize_dispatch_ref(raw: &str) -> String {
    let mut trimmed = raw.trim();
    if trimmed.is_empty() {
        trimmed = "main";
    }
    if trimmed.starts_with("refs/") {
        return trimmed.to_string();
    }
    format!("refs/heads/{trimmed}")
}

fn parse_workflow_inputs(
    pairs: &[String],
) -> eyre::Result<serde_json::Map<String, serde_json::Value>> {
    let mut out: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for s in pairs {
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| eyre!("Invalid --input '{}'. Expected KEY=VALUE.", s))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(eyre!("Invalid --input '{}'. KEY must be non-empty.", s));
        }
        out.insert(
            key.to_string(),
            serde_json::Value::String(value.trim().to_string()),
        );
    }
    Ok(out)
}

fn parse_optional_duration(raw: Option<&str>) -> eyre::Result<Option<Duration>> {
    raw.map(parse_duration).transpose()
}

fn parse_duration(raw: &str) -> eyre::Result<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(eyre!("duration cannot be empty"));
    }

    let number_len = raw
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .map(|(idx, ch)| idx + ch.len_utf8())
        .last()
        .unwrap_or(0);
    if number_len == 0 {
        return Err(eyre!(
            "invalid duration '{raw}'. Expected a number followed by s, m, h, or d."
        ));
    }

    let amount: u64 = raw[..number_len]
        .parse()
        .wrap_err_with(|| format!("invalid duration '{raw}'"))?;
    if amount == 0 {
        return Err(eyre!("duration must be greater than zero"));
    }

    let unit = raw[number_len..].trim().to_ascii_lowercase();
    let seconds_per_unit = match unit.as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        _ => {
            return Err(eyre!(
                "invalid duration unit '{}' in '{}'. Allowed units: s, m, h, d.",
                unit,
                raw
            ))
        }
    };

    let seconds = amount
        .checked_mul(seconds_per_unit)
        .ok_or_else(|| eyre!("duration '{raw}' is too large"))?;
    Ok(Duration::from_secs(seconds))
}

fn normalize_run_status_filter(raw: &str) -> eyre::Result<String> {
    let normalized = normalize_run_status(raw);
    let allowed = [
        "success", "failure", "running", "waiting", "canceled", "skipped", "blocked",
    ];
    if !allowed.contains(&normalized.as_str()) {
        return Err(eyre!(
            "Unknown --status '{}'. Allowed: {}",
            raw,
            allowed.join(", ")
        ));
    }
    Ok(normalized)
}

fn normalize_run_status(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    if s == "cancelled" {
        s = "canceled".to_string();
    }
    s
}

fn is_run_done_from_jobs(jobs: &[crate::ui_actions::JobInfo]) -> bool {
    let in_progress = ["running", "queued", "pending", "waiting"];
    !jobs.iter().any(|j| {
        j.status.as_deref().map_or(true, |status| {
            in_progress.iter().any(|s| status.eq_ignore_ascii_case(s))
        })
    })
}

fn run_terminal_error_from_jobs(
    run_index: i64,
    jobs: &[crate::ui_actions::JobInfo],
) -> Option<eyre::Report> {
    if jobs.is_empty() {
        return Some(eyre!("run {run_index} returned no jobs"));
    }

    let failing = ["failure", "canceled", "cancelled", "blocked"];
    let mut failures: Vec<String> = Vec::new();
    for j in jobs {
        let Some(status) = j.status.as_deref() else {
            continue;
        };
        if failing.iter().any(|s| status.eq_ignore_ascii_case(s)) {
            failures.push(format!("job {}: {}", j.job_index, status));
        }
    }

    if failures.is_empty() {
        None
    } else {
        Some(eyre!(
            "run {run_index} finished with failures:\n - {}",
            failures.join("\n - ")
        ))
    }
}

fn safe_filename_component(job_name: &str, job_index: i64) -> String {
    let mut safe = SAFE_FILENAME_COMPONENT_RE
        .replace_all(job_name, "_")
        .to_string();
    safe = safe.trim_matches('_').to_string();
    if safe.is_empty() {
        safe = format!("job-{job_index}");
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(job_index: i64, status: Option<&str>) -> crate::ui_actions::JobInfo {
        crate::ui_actions::JobInfo {
            run_index: 10,
            job_index,
            id: Some(job_index + 100),
            name: Some(format!("job-{job_index}")),
            status: status.map(str::to_string),
            can_rerun: None,
            duration: None,
        }
    }

    #[test]
    fn parse_duration_accepts_seconds_minutes_hours_and_days() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7_200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
    }

    #[test]
    fn parse_duration_rejects_empty_zero_and_unknown_units() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("1w").is_err());
        assert!(parse_duration("1.5m").is_err());
    }

    #[test]
    fn wait_target_done_requires_terminal_statuses() {
        let running = vec![job(0, Some("success")), job(1, Some("running"))];
        assert!(!is_wait_target_done(&running, None));

        let done = vec![job(0, Some("success")), job(1, Some("skipped"))];
        assert!(is_wait_target_done(&done, None));
        assert!(is_wait_target_done(&done, Some(0)));
    }

    #[test]
    fn wait_terminal_error_reports_failed_run_or_job() {
        let jobs = vec![job(0, Some("success")), job(1, Some("failure"))];
        assert!(wait_terminal_error(&jobs, 10, None).is_some());
        assert!(wait_terminal_error(&jobs, 10, Some(1)).is_some());
        assert!(wait_terminal_error(&jobs, 10, Some(0)).is_none());
    }
}
