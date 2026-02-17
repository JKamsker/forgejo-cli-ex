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
        ActionsSubcommand::Workflows { json } => {
            let workflows = crate::ui_actions::list_workflows(&session, &repo, 1).await?;
            if json {
                let payload = serde_json::json!({
                    "baseUrl": target.base_url,
                    "repo": repo,
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
            page,
            limit,
            json,
        } => {
            let runs =
                crate::ui_actions::list_runs(&session, &repo, workflow.as_deref(), page, limit)
                    .await?;

            if json {
                let payload = serde_json::json!({
                    "baseUrl": target.base_url,
                    "repo": repo,
                    "page": page,
                    "limit": limit,
                    "workflow": workflow,
                    "runs": runs.iter().map(|r| serde_json::json!({"runIndex": r.run_index, "url": r.url})).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }

            println!("RunIndex\tUrl");
            for r in runs {
                println!("{}\t{}", r.run_index, r.url);
            }
        }
        ActionsSubcommand::Jobs {
            run_index,
            latest,
            json,
        } => {
            let run_index = match (run_index, latest) {
                (Some(n), false) if n > 0 => n,
                _ => crate::ui_actions::latest_run_index(&session, &repo).await?,
            };

            let view = crate::ui_actions::get_run_view_data(&session, &repo, run_index)
                .await
                .wrap_err("failed to load run view")?;
            let jobs = crate::ui_actions::get_run_jobs(run_index, &view.view)?;

            if json {
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
                return Ok(());
            }

            println!("JobIndex\tStatus\tName");
            for j in jobs {
                let status = j.status.as_deref().unwrap_or("?");
                let name = j.name.as_deref().unwrap_or("");
                println!("{}\t{}\t{}", j.job_index, status, name);
            }
        }
        ActionsSubcommand::Logs { command } => match command {
            ActionsLogsSubcommand::Job {
                run_index,
                job_index,
                attempt,
                out_file,
            } => {
                let attempt = match attempt {
                    Some(a) if a > 0 => a,
                    _ => {
                        crate::ui_actions::get_job_view_meta(&session, &repo, run_index, job_index)
                            .await?
                            .attempt_number
                    }
                };

                let repo_path = repo.trim_matches('/');
                let logs_url = format!(
                    "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/attempt/{attempt}/logs",
                    session.base_url()
                );

                let resp = session.get_response(&logs_url, true).await?;
                if resp.status() != reqwest::StatusCode::OK {
                    return Err(eyre!(
                        "Failed to download logs from '{}'. HTTP {}.",
                        logs_url,
                        resp.status()
                    ));
                }
                let bytes = resp.bytes().await.wrap_err("failed to read log bytes")?;

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
                out_dir,
                max_jobs,
            } => {
                let run_index = match (run_index, latest) {
                    (Some(n), false) if n > 0 => n,
                    _ => crate::ui_actions::latest_run_index(&session, &repo).await?,
                };

                let view = crate::ui_actions::get_run_view_data(&session, &repo, run_index)
                    .await
                    .wrap_err("failed to load run view")?;
                let mut jobs = crate::ui_actions::get_run_jobs(run_index, &view.view)?;

                if max_jobs > 0 && (jobs.len() as u32) > max_jobs {
                    jobs.truncate(max_jobs as usize);
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

                        let repo_path = repo.trim_matches('/');
                        let logs_url = format!(
                            "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/attempt/{attempt}/logs",
                            session.base_url()
                        );
                        match session.get_response(&logs_url, true).await {
                            Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
                                let bytes =
                                    resp.bytes().await.wrap_err("failed to read log bytes")?;
                                tokio::fs::write(&out_file, &bytes).await?;
                                println!(
                                    "Saved: job {} (attempt {}) -> {}",
                                    job_index,
                                    attempt,
                                    out_file.display()
                                );
                            }
                            Ok(resp) => {
                                let msg =
                                    format!("Job {job_index} ({job_name}): HTTP {}", resp.status());
                                eprintln!("warn: {msg}");
                                failures.push(msg);
                            }
                            Err(e) => {
                                let msg = format!("Job {job_index} ({job_name}): {e}");
                                eprintln!("warn: {msg}");
                                failures.push(msg);
                            }
                        }
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

                    let repo_path = repo.trim_matches('/');
                    let logs_url = format!(
                        "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/attempt/{attempt}/logs",
                        session.base_url()
                    );

                    let resp = session.get_response(&logs_url, true).await?;
                    if resp.status() != reqwest::StatusCode::OK {
                        return Err(eyre!(
                            "Failed to download logs from '{}'. HTTP {}.",
                            logs_url,
                            resp.status()
                        ));
                    }
                    let bytes = resp.bytes().await.wrap_err("failed to read log bytes")?;

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
                json,
            } => {
                let run_index = match (run_index, latest) {
                    (Some(n), false) if n > 0 => n,
                    _ => crate::ui_actions::latest_run_index(&session, &repo).await?,
                };

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

                println!("Id\tName\tSize");
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
                    println!("{id}\t{name}\t{size}");
                }
            }
            ActionsArtifactsSubcommand::Get {
                run_index,
                artifact,
                out_file,
            } => {
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
        ActionsSubcommand::Cancel { run_index, dry_run } => {
            let repo_path = repo.trim_matches('/');
            let url = format!(
                "{}/{repo_path}/actions/runs/{run_index}/cancel",
                session.base_url()
            );

            if dry_run {
                println!("DRY RUN: POST {url}");
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
            println!("Canceled run #{run_index}");
        }
        ActionsSubcommand::Rerun {
            run_index,
            job_index,
            dry_run,
        } => {
            let repo_path = repo.trim_matches('/');
            let url = match job_index {
                Some(job_index) if job_index >= 0 => format!(
                    "{}/{repo_path}/actions/runs/{run_index}/jobs/{job_index}/rerun",
                    session.base_url()
                ),
                _ => format!(
                    "{}/{repo_path}/actions/runs/{run_index}/rerun",
                    session.base_url()
                ),
            };

            if dry_run {
                println!("DRY RUN: POST {url}");
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
            if !text.trim().is_empty() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(redirect) = v.get("redirect").and_then(|v| v.as_str()) {
                        println!("{redirect}");
                        return Ok(());
                    }
                }
            }

            println!("Rerun requested.");
        }
    }

    Ok(())
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
