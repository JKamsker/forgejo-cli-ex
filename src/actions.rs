use std::io::IsTerminal;
use std::num::NonZeroU64;

use eyre::{eyre, Context};
use tokio::io::AsyncWriteExt;

use crate::cli::{
    ActionsArtifactsSubcommand, ActionsCommand, ActionsLogsSubcommand, ActionsSubcommand,
};

pub async fn run(args: ActionsCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;

    let repo = target.repo.ok_or_else(|| {
        eyre!(
            "Repo could not be resolved. Pass --repo owner/name or run inside a git repo with a Forgejo remote."
        )
    })?;

    let session = crate::session::UiSession::from_store(&target.base_url, false).await?;

    match args.command {
        ActionsSubcommand::Workflows { page, limit, json } => {
            let workflows = crate::ui_actions::list_workflows(&session, &repo, page, limit).await?;
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
            let mut runs =
                crate::ui_actions::list_runs(&session, &repo, workflow.as_deref(), page, limit)
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
                resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref()).await?;

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
                        let show_header = crate::output::should_print_header(header, no_header);
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
        ActionsSubcommand::Logs { command } => match command {
            ActionsLogsSubcommand::Job {
                run_index,
                latest,
                workflow,
                job_index,
                attempt,
                out_file,
            } => {
                let run_index =
                    resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
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
                let bytes = crate::ui_actions::download_job_logs(
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
                    tokio::fs::write(&out_file, &bytes).await?;
                    println!("{}", out_file.display());
                } else {
                    let mut stdout = tokio::io::stdout();
                    stdout.write_all(&bytes).await?;
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
                let run_index =
                    resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
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
                        let out_file = out_dir.join(format!("job-{job_index}-{safe_name}.log"));

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
                        match crate::ui_actions::download_job_logs(
                            &session, &repo, run_index, job_index, attempt,
                        )
                        .await
                        {
                            Ok(bytes) => {
                                tokio::fs::write(&out_file, &bytes).await?;
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
                        return Err(eyre!("Some jobs failed:\n - {}", failures.join("\n - ")));
                    }

                    return Ok(());
                }

                // stdout mode
                for job in &jobs {
                    let job_index = job.job_index;
                    let job_name = job.name.as_deref().unwrap_or("");

                    let attempt =
                        crate::ui_actions::get_job_view_meta(&session, &repo, run_index, job_index)
                            .await?
                            .attempt_number;

                    eprintln!(
                        "== job {} (attempt {}) :: {} ==",
                        job_index, attempt, job_name
                    );
                    let bytes = crate::ui_actions::download_job_logs(
                        &session, &repo, run_index, job_index, attempt,
                    )
                    .await?;

                    let mut stdout = tokio::io::stdout();
                    stdout.write_all(&bytes).await?;
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
                let run_index =
                    resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
                        .await?;

                let artifacts =
                    crate::ui_actions::get_run_artifacts(&session, &repo, run_index).await?;

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
                    .cloned()
                    .unwrap_or_default();

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
                let run_index =
                    resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref())
                        .await?;
                let bytes =
                    crate::ui_actions::download_artifact(&session, &repo, run_index, &artifact)
                        .await?;
                if let Some(parent) = out_file.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&out_file, bytes).await?;
                println!("{}", out_file.display());
            }
        },
        ActionsSubcommand::Trigger {
            workflow,
            git_ref,
            input,
            dry_run,
            json,
        } => {
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

            let dispatch_ref = normalize_dispatch_ref(&git_ref);
            let inputs = parse_workflow_inputs(&input)?;

            let url = format!(
                "{}/api/v1/repos/{}/{}/actions/workflows/{}/dispatches",
                target.base_url.trim_end_matches('/'),
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

            let client = reqwest::Client::builder()
                .user_agent(concat!(
                    env!("CARGO_PKG_NAME"),
                    "/",
                    env!("CARGO_PKG_VERSION")
                ))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .wrap_err("failed to build http client")?;

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
            } else {
                println!("Triggered workflow '{workflow}' on '{dispatch_ref}'.");
            }
        }
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
                resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref()).await?;
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
                resolve_run_index(&session, &repo, run_index, latest, workflow.as_deref()).await?;
            let repo_path = repo.trim_matches('/');

            if failed_only {
                let view = crate::ui_actions::get_run_view_data(&session, &repo, run_index)
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
                            "baseUrl": target.base_url,
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
                            "baseUrl": target.base_url,
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
                    let resp = session.post_json_response(&url, &body, true).await?;
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
                    return Err(eyre!(
                        "Some job reruns failed:\n - {}",
                        failures.join("\n - ")
                    ));
                }

                if json {
                    let payload = serde_json::json!({
                        "baseUrl": target.base_url,
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

                return Ok(());
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
                Some(job_index) => println!("Rerun requested for run #{run_index}, job #{job_index}."),
                None => println!("Rerun requested for run #{run_index}."),
            }
            if let Some(redirect) = redirect {
                println!("{redirect}");
            }
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
    let trimmed = raw.trim();
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
        let Some(status) = j.status.as_deref() else {
            return false;
        };
        in_progress.iter().any(|s| status.eq_ignore_ascii_case(s))
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
    let re = regex::Regex::new(r#"[^a-zA-Z0-9._-]+"#).expect("valid regex");
    let mut safe = re.replace_all(job_name, "_").to_string();
    safe = safe.trim_matches('_').to_string();
    if safe.is_empty() {
        safe = format!("job-{job_index}");
    }
    safe
}
