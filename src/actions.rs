use std::io::{IsTerminal, Read};
use std::num::NonZeroU64;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eyre::{eyre, Context};
use tokio::io::AsyncWriteExt;

use crate::cli::{
    ActionsArtifactsSubcommand, ActionsCommand, ActionsLogsSubcommand, ActionsRunnersSubcommand,
    ActionsSecretsSubcommand, ActionsSubcommand, ActionsVariablesSubcommand, RunnerScope,
};

static SAFE_FILENAME_COMPONENT_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"[^a-zA-Z0-9._-]+"#).expect("valid regex"));
const IN_PROGRESS_JOB_STATUSES: &[&str] =
    &["running", "queued", "pending", "waiting", "cancelling"];
const FAILING_JOB_STATUSES: &[&str] = &["failure", "canceled", "cancelled", "blocked"];

#[derive(Debug)]
struct WaitResult {
    run_index: i64,
    run_url: String,
    /// The run-list status, set only once it was observed as terminal (it then
    /// overrides a possibly stale job snapshot).
    terminal_run_status: Option<String>,
    jobs: Vec<crate::ui_actions::JobInfo>,
}

pub async fn run(args: ActionsCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let backend = args.backend;

    match args.command {
        ActionsSubcommand::Secrets { command } => {
            run_secrets(command, &target).await?;
        }
        ActionsSubcommand::Variables { command } => {
            run_variables(command, &target).await?;
        }
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

            let wait_client = if wait {
                Some(crate::actions_backend::connect(backend, &target, &repo).await?)
            } else {
                None
            };

            let wait_workflow_filter = workflow_filter_for_wait(workflow);
            let previous_latest_run = if let Some(client) = wait_client.as_ref() {
                client
                    .latest_run_index_optional(&repo, wait_workflow_filter)
                    .await?
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

            let auth = crate::credentials::resolve_api_auth(&target.base_url)
                .await?
                .ok_or_else(|| crate::credentials::missing_api_auth_error(&target.base_url))?;

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

            let resp = auth
                .apply(client.post(&url).json(&body))?
                .send()
                .await
                .wrap_err("trigger request failed")?;

            let status = resp.status();
            if status != reqwest::StatusCode::NO_CONTENT
                && status != reqwest::StatusCode::OK
                && status != reqwest::StatusCode::CREATED
            {
                // The response body is not included: it may echo request data.
                return Err(eyre!(
                    "Trigger failed: HTTP {} (authenticated with the {}).{}",
                    status,
                    auth.source_label(),
                    match status.as_u16() {
                        401 => " The credential was rejected.",
                        403 => " The credential lacks permission (tokens need the write:repository scope).",
                        404 => " Check the repository and workflow file name.",
                        _ => "",
                    }
                ));
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
                let session = wait_client
                    .as_ref()
                    .expect("wait client must exist when --wait is set");
                let wait_started_at = Instant::now();
                let run_index = wait_for_new_run_index(
                    session,
                    &repo,
                    wait_workflow_filter,
                    previous_latest_run,
                    &dispatch_ref,
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
                if let Some(err) = wait_terminal_error(&result, None) {
                    return Err(err);
                }
            }
        }
        ActionsSubcommand::SmokeTest(args) => {
            // The smoke test exercises the web UI endpoints and builds its own session.
            let repo = require_repo_owned(&target)?;
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
        command => {
            let repo = require_repo_owned(&target)?;
            let client = crate::actions_backend::connect(backend, &target, &repo).await?;
            let session = &client;

            match command {
                ActionsSubcommand::Workflows { page, limit, json } => {
                    let workflows = session.list_workflows(&repo, page, limit).await?;
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
                        println!("{}", crate::output::sanitize_line(&w));
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
                    let mut runs = session
                        .list_runs(&repo, workflow.as_deref(), page, limit)
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
                    json_lines,
                } => {
                    let run_index =
                        resolve_run_index(session, &repo, run_index, latest, workflow.as_deref())
                            .await?;

                    if watch && watch_interval == 0 {
                        return Err(eyre!("--watch-interval must be >= 1"));
                    }

                    let interactive = std::io::stdout().is_terminal();
                    let mut last_sig: Option<String> = None;
                    let mut run_status_tracker = RunStatusTracker::new(run_index);

                    loop {
                        // The job state embedded in a run page can lag behind the Actions
                        // list. The list is the authoritative source for a terminal run.
                        let run_status = run_status_tracker.poll(&session, &repo).await?;
                        let jobs = session.run_jobs(&repo, run_index).await?;

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

                        let done = run_status_tracker.is_terminal()
                            || (run_status.is_none() && is_run_done_from_jobs(&jobs));

                        // For watch, only print intermediate updates when interactive. JSON
                        // Lines is also a streaming interface, but it emits only state deltas
                        // rather than an identical full snapshot on every polling interval.
                        // Always retain the final terminal snapshot.
                        let should_print = should_print_jobs_snapshot(
                            watch,
                            done,
                            interactive,
                            json_lines,
                            changed,
                        );

                        if should_print {
                            // Clearing the screen is for the human table only; it would
                            // corrupt a JSON Lines stream read from a terminal.
                            if interactive && watch && !json && !json_lines {
                                use std::io::Write;
                                print!("\x1B[2J\x1B[H");
                                let _ = std::io::stdout().flush();
                            }

                            if json || json_lines {
                                // The default JSON mode prints a final snapshot. JSON lines is
                                // deliberately opt-in for streaming parsers and emits only
                                // initial, changed, and terminal observations.
                                if !watch || done || (json_lines && changed) {
                                    let payload = serde_json::json!({
                                        "baseUrl": target.base_url,
                                        "repo": repo,
                                        "runIndex": run_index,
                                        "runStatus": run_status,
                                        "observedAtUnixMs": observed_at_unix_ms(),
                                        "terminalObservations": run_status_tracker.terminal_observations,
                                        "complete": done,
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
                                    println!("{}", format_jobs_snapshot(&payload, json_lines)?);
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
                            if let Some(err) =
                                run_terminal_error(run_index, run_status.as_deref(), &jobs)
                            {
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
                        resolve_run_index(session, &repo, run_index, latest, workflow.as_deref())
                            .await?;
                    let timeout = parse_optional_duration(timeout.as_deref())?;
                    let interval = Duration::from_secs(interval);
                    let job_index = job_index.map(i64::from);

                    let result = wait_for_run_completion(
                        session, &repo, run_index, job_index, interval, timeout,
                    )
                    .await?;
                    print_wait_result(&target.base_url, &repo, &result, job_index, json, None)?;
                    if let Some(err) = wait_terminal_error(&result, job_index) {
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
                        follow,
                        follow_interval,
                    } => {
                        let run_index = resolve_run_index(
                            session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;
                        let job_index_i64 = job_index as i64;
                        if follow {
                            // Errors that retrying cannot fix are reported before
                            // following; inside the loop every open error is taken
                            // to mean the job has not started yet.
                            session.check_job_logs_supported()?;
                            let jobs = session.run_jobs(&repo, run_index).await?;
                            if let Some(err) =
                                follow_missing_job_error(&jobs, run_index, job_index_i64)
                            {
                                return Err(err);
                            }
                        }
                        let mut previous = Vec::new();
                        let mut run_status_tracker = RunStatusTracker::new(run_index);
                        let mut announced_wait = false;
                        // Set once the run is known to be finished from the job snapshot
                        // fallback: one more log fetch picks up the final bytes.
                        let mut finishing: Option<Option<eyre::Report>> = None;
                        loop {
                            let opened = async {
                                let attempt = match attempt {
                                    Some(a) => a.get() as i64,
                                    None => {
                                        session.job_attempt(&repo, run_index, job_index_i64).await?
                                    }
                                };
                                session
                                    .open_job_logs(&repo, run_index, job_index_i64, attempt)
                                    .await
                            }
                            .await;
                            let resp = match opened {
                                Ok(resp) => resp,
                                // A job that is still queued has no log yet. Keep
                                // following until it starts or the run ends.
                                Err(err) if follow => {
                                    let run_status =
                                        run_status_tracker.poll(&session, &repo).await?;
                                    if !follow_should_wait_for_log(
                                        run_status.as_deref(),
                                        run_status_tracker.is_terminal(),
                                    ) {
                                        return Err(err);
                                    }
                                    if !std::mem::replace(&mut announced_wait, true) {
                                        eprintln!(
                                            "waiting for job {job_index_i64} of run #{run_index} to produce a log (run status: {})",
                                            crate::output::sanitize_line(
                                                run_status.as_deref().unwrap_or("unknown")
                                            )
                                        );
                                    }
                                    tokio::time::sleep(std::time::Duration::from_secs(
                                        follow_interval,
                                    ))
                                    .await;
                                    continue;
                                }
                                Err(err) => return Err(err),
                            };

                            if !follow {
                                if let Some(out_file) = out_file.as_ref() {
                                    if let Some(parent) = out_file.parent() {
                                        tokio::fs::create_dir_all(parent).await?;
                                    }
                                    let mut file = tokio::fs::File::create(out_file).await?;
                                    crate::ui_actions::copy_response_body(resp, &mut file).await?;
                                    file.flush().await?;
                                    println!("{}", out_file.display());
                                } else {
                                    let mut stdout = tokio::io::stdout();
                                    crate::ui_actions::copy_response_body(resp, &mut stdout)
                                        .await?;
                                    stdout.flush().await?;
                                }
                                break;
                            }

                            let current = resp
                                .bytes()
                                .await
                                .wrap_err("failed to read job log response")?
                                .to_vec();
                            let (delta, reset) = log_delta(&previous, &current);
                            if reset {
                                eprintln!("== job log was replaced; replaying current snapshot ==");
                            }
                            if !delta.is_empty() {
                                let mut stdout = tokio::io::stdout();
                                stdout.write_all(delta).await?;
                                stdout.flush().await?;
                            }
                            previous = current;

                            if let Some(final_error) = finishing.take() {
                                if let Some(err) = final_error {
                                    return Err(err);
                                }
                                break;
                            }

                            let run_status = run_status_tracker.poll(&session, &repo).await?;
                            if run_status_tracker.is_terminal() {
                                if let Some(err) =
                                    run_terminal_error(run_index, run_status.as_deref(), &[])
                                {
                                    return Err(err);
                                }
                                break;
                            }
                            if run_status.is_none() {
                                let jobs = session.run_jobs(&repo, run_index).await?;
                                if is_run_done_from_jobs(&jobs) {
                                    finishing = Some(run_terminal_error(run_index, None, &jobs));
                                    continue;
                                }
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(follow_interval))
                                .await;
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
                            session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;

                        let mut jobs = session.run_jobs(&repo, run_index).await?;

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

                                let attempt =
                                    match session.job_attempt(&repo, run_index, job_index).await {
                                        Ok(attempt) => attempt,
                                        Err(e) => {
                                            let msg = format!(
                                                "Job {job_index} ({}): {e}",
                                                crate::output::sanitize_line(job_name)
                                            );
                                            eprintln!("warn: {msg}");
                                            failures.push(msg);
                                            continue;
                                        }
                                    };
                                match session
                                    .open_job_logs(&repo, run_index, job_index, attempt)
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
                                        let msg = format!(
                                            "Job {job_index} ({}): {e}",
                                            crate::output::sanitize_line(job_name)
                                        );
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

                            let attempt = session.job_attempt(&repo, run_index, job_index).await?;

                            eprintln!(
                                "== job {} (attempt {}) :: {} ==",
                                job_index,
                                attempt,
                                crate::output::sanitize_line(job_name)
                            );
                            let resp = session
                                .open_job_logs(&repo, run_index, job_index, attempt)
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
                            session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;

                        let artifacts = session.run_artifacts(&repo, run_index).await?;
                        if artifact_items(&artifacts).is_empty() {
                            if let Some(hint) = session.empty_artifacts_hint() {
                                eprintln!("{hint}");
                            }
                        }

                        if json {
                            let items = artifact_items(&artifacts);
                            let payload = serde_json::json!({
                                "baseUrl": target.base_url,
                                "repo": repo,
                                "runIndex": run_index,
                                "artifactCount": items.len(),
                                "artifacts": items,
                            });
                            println!("{}", serde_json::to_string_pretty(&payload)?);
                            return Ok(());
                        }

                        let items = artifact_items(&artifacts);

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
                            session,
                            &repo,
                            run_index,
                            latest,
                            workflow.as_deref(),
                        )
                        .await?;
                        let resp = session.open_artifact(&repo, run_index, &artifact).await?;
                        if let Some(parent) = out_file.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        let mut file = tokio::fs::File::create(&out_file).await?;
                        crate::ui_actions::copy_response_body(resp, &mut file).await?;
                        file.flush().await?;
                        println!("{}", out_file.display());
                    }
                },
                ActionsSubcommand::Cancel {
                    run_index,
                    latest,
                    workflow,
                    dry_run,
                    json,
                } => {
                    let run_index =
                        resolve_run_index(session, &repo, run_index, latest, workflow.as_deref())
                            .await?;
                    if dry_run {
                        let url = session.cancel_url(&repo, run_index).await?;
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

                    let url = session.cancel(&repo, run_index).await?;

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
                        resolve_run_index(session, &repo, run_index, latest, workflow.as_deref())
                            .await?;
                    if failed_only {
                        return rerun_failed_jobs(
                            session,
                            target.base_url.as_str(),
                            &repo,
                            run_index,
                            dry_run,
                            json,
                        )
                        .await;
                    }

                    let job_index_i64 = job_index.map(i64::from);

                    if dry_run {
                        let url = session.rerun_url(&repo, run_index, job_index_i64).await?;
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

                    let (url, redirect) = session.rerun(&repo, run_index, job_index_i64).await?;

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
                        println!("{}", crate::output::sanitize_line(&redirect));
                    }
                }
                ActionsSubcommand::Trigger { .. } | ActionsSubcommand::SmokeTest(_) => {
                    unreachable!("handled before backend initialization")
                }
                ActionsSubcommand::Runners { .. } => {
                    unreachable!("Runners is handled before UiSession initialization")
                }
                ActionsSubcommand::Secrets { .. } => {
                    unreachable!("Secrets is handled before UiSession initialization")
                }
                ActionsSubcommand::Variables { .. } => {
                    unreachable!("Variables is handled before UiSession initialization")
                }
            }
        }
    }

    Ok(())
}

async fn run_secrets(
    command: ActionsSecretsSubcommand,
    target: &crate::target::ResolvedTarget,
) -> eyre::Result<()> {
    let ActionsSecretsSubcommand::Set { name, value_stdin } = command;
    run_repository_actions_value(target, &name, value_stdin, "secrets", "data", "secret").await
}

async fn run_variables(
    command: ActionsVariablesSubcommand,
    target: &crate::target::ResolvedTarget,
) -> eyre::Result<()> {
    let ActionsVariablesSubcommand::Set { name, value_stdin } = command;
    run_repository_actions_value(target, &name, value_stdin, "variables", "value", "variable").await
}

async fn run_repository_actions_value(
    target: &crate::target::ResolvedTarget,
    name: &str,
    value_stdin: bool,
    resource_path: &str,
    value_field: &str,
    resource_label: &str,
) -> eyre::Result<()> {
    if !value_stdin {
        return Err(eyre!(
            "Pass --value-stdin; literal secret values are intentionally unsupported."
        ));
    }

    let actions_name = validate_actions_name(name)?;
    let actions_value = read_actions_value_from_stdin()?;
    let repo = require_repo_owned(target)?;
    let (owner, repo_name) = repo.trim_matches('/').split_once('/').ok_or_else(|| {
        eyre!(
            "Repo should be in the format owner/name; got '{}'. Pass --repo owner/name.",
            repo
        )
    })?;

    // Unix-socket targets must use the HTTP base while reqwest carries the socket path.
    let request_base = if target.base_url.starts_with("http+unix://") {
        "http://localhost"
    } else {
        target.base_url.trim_end_matches('/')
    };
    let url = format!(
        "{}/api/v1/repos/{}/{}/actions/{}/{}",
        request_base,
        urlencoding::encode(owner),
        urlencoding::encode(repo_name),
        resource_path,
        urlencoding::encode(&actions_name)
    );

    let auth = crate::credentials::resolve_api_auth(&target.base_url)
        .await?
        .ok_or_else(|| crate::credentials::missing_api_auth_error(&target.base_url))?;
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
    let mut body = serde_json::Map::new();
    body.insert(
        value_field.to_string(),
        serde_json::Value::String(actions_value),
    );
    let mut response = auth
        .apply(client.put(&url).json(&body))?
        .send()
        .await
        .wrap_err("Actions secret update request failed")?;

    // Forgejo/Gitea create-or-update secrets with PUT, but PUT on a variable
    // only updates an existing one (404 otherwise); variables are created with POST.
    if resource_path == "variables" && response.status() == reqwest::StatusCode::NOT_FOUND {
        response = auth
            .apply(client.post(&url).json(&body))?
            .send()
            .await
            .wrap_err("Actions variable create request failed")?;
    }

    if !matches!(
        response.status(),
        reqwest::StatusCode::CREATED | reqwest::StatusCode::NO_CONTENT
    ) {
        // Never include a remote response body here: it may echo sensitive input.
        return Err(eyre!(
            "Actions {} update failed for '{}': HTTP {} (authenticated with the {}).",
            resource_label,
            actions_name,
            response.status(),
            auth.source_label()
        ));
    }

    println!(
        "Updated Actions {} '{}' for '{}'.",
        resource_label, actions_name, repo
    );
    Ok(())
}

fn validate_actions_name(name: &str) -> eyre::Result<String> {
    let name = name.trim();
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic());
    if !valid_start || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        return Err(eyre!(
            "Invalid Actions secret name '{}'. Use ASCII letters, digits, and underscores; the first character must be a letter or underscore.",
            name
        ));
    }
    Ok(name.to_string())
}

fn read_actions_value_from_stdin() -> eyre::Result<String> {
    if std::io::stdin().is_terminal() {
        return Err(eyre!("--value-stdin requires a piped secret value."));
    }

    let mut value = String::new();
    std::io::stdin()
        .read_to_string(&mut value)
        .wrap_err("failed to read Actions secret from standard input")?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.is_empty() {
        return Err(eyre!(
            "Actions secret value from standard input must not be empty."
        ));
    }
    Ok(value)
}

async fn wait_for_new_run_index(
    session: &crate::actions_backend::ActionsClient,
    repo: &str,
    workflow: Option<&str>,
    previous_latest_run: Option<i64>,
    dispatch_ref: &str,
    interval: Duration,
    timeout: Option<Duration>,
) -> eyre::Result<i64> {
    let started_at = Instant::now();

    loop {
        let runs = session.list_runs(repo, workflow, 1, 20).await?;
        if let Some(run_index) = select_dispatched_run(&runs, previous_latest_run, dispatch_ref) {
            return Ok(run_index);
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

/// Picks the run created by a `workflow_dispatch`: a run newer than the latest
/// run seen before dispatching, whose ref label (when the run list shows one)
/// matches the dispatched ref. Of several candidates the oldest is chosen:
/// Forgejo creates the dispatched run before the dispatch request returns, so
/// runs started concurrently afterwards receive higher indexes.
fn select_dispatched_run(
    runs: &[crate::ui_actions::RunRef],
    previous_latest_run: Option<i64>,
    dispatch_ref: &str,
) -> Option<i64> {
    runs.iter()
        .filter(|run| previous_latest_run.is_none_or(|previous| run.run_index > previous))
        .filter(|run| run_ref_matches_dispatch(run.branch.as_deref(), dispatch_ref))
        .map(|run| run.run_index)
        .min()
}

/// The run list labels runs with a short ref (`main`, `v1.0`), while dispatch
/// uses a full ref (`refs/heads/main`). A run without a label is not excluded.
fn run_ref_matches_dispatch(run_ref: Option<&str>, dispatch_ref: &str) -> bool {
    let Some(run_ref) = run_ref.map(str::trim).filter(|run_ref| !run_ref.is_empty()) else {
        return true;
    };
    let short_ref = dispatch_ref
        .strip_prefix("refs/heads/")
        .or_else(|| dispatch_ref.strip_prefix("refs/tags/"))
        .unwrap_or(dispatch_ref);
    run_ref == short_ref || run_ref == dispatch_ref
}

async fn wait_for_run_completion(
    session: &crate::actions_backend::ActionsClient,
    repo: &str,
    run_index: i64,
    job_index: Option<i64>,
    interval: Duration,
    timeout: Option<Duration>,
) -> eyre::Result<WaitResult> {
    let started_at = Instant::now();
    let mut run_status_tracker = RunStatusTracker::new(run_index);

    loop {
        // As for `jobs --watch`, the run list is authoritative for a terminal
        // run; the run page's job snapshot is used for single jobs and as a
        // fallback when the run is not listed.
        let run_status = run_status_tracker.poll(session, repo).await?;
        let jobs = session.run_jobs(repo, run_index).await?;

        let run_terminal = run_status_tracker.is_terminal();
        if is_wait_target_done(&jobs, job_index, run_status.as_deref(), run_terminal) {
            return Ok(WaitResult {
                run_index,
                run_url: session.run_url(repo, run_index),
                terminal_run_status: run_status.filter(|_| run_terminal),
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
    let status = wait_target_status(result, job_index);

    if json {
        let mut payload = serde_json::json!({
            "baseUrl": target_base_url,
            "repo": repo,
            "runIndex": result.run_index,
            "runUrl": result.run_url,
            "jobIndex": job_index,
            "status": status,
            "runStatus": result.terminal_run_status,
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

    let status = crate::output::sanitize_line(&status);
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

/// Whether a wait can stop.
///
/// `run_status` is the run-list status (`None` when the run is not listed) and
/// `run_terminal` whether it has been terminal for two consecutive polls. A
/// terminal run ends any wait; while the run is still active only the selected
/// job finishing ends a job wait early. Without a run-list status the job
/// snapshot decides, as before.
fn is_wait_target_done(
    jobs: &[crate::ui_actions::JobInfo],
    job_index: Option<i64>,
    run_status: Option<&str>,
    run_terminal: bool,
) -> bool {
    if run_terminal {
        return true;
    }
    if run_status.is_none() {
        return is_snapshot_wait_target_done(jobs, job_index);
    }
    job_index.is_some_and(|job_index| find_job(jobs, job_index).is_some_and(is_job_finished))
}

fn is_snapshot_wait_target_done(
    jobs: &[crate::ui_actions::JobInfo],
    job_index: Option<i64>,
) -> bool {
    if jobs.is_empty() {
        return false;
    }

    if let Some(job_index) = job_index {
        if let Some(job) = find_job(jobs, job_index) {
            return is_job_done(job);
        }
        return is_run_done_from_jobs(jobs);
    }

    is_run_done_from_jobs(jobs)
}

fn find_job(
    jobs: &[crate::ui_actions::JobInfo],
    job_index: i64,
) -> Option<&crate::ui_actions::JobInfo> {
    jobs.iter().find(|job| job.job_index == job_index)
}

fn is_job_done(job: &crate::ui_actions::JobInfo) -> bool {
    job.status
        .as_deref()
        .is_some_and(|status| !is_in_progress_status(status))
}

/// A job that has finished while its run is still active. Unlike the snapshot
/// fallback, `blocked` is not final here: Forgejo/Gitea also use it for jobs
/// whose `needs` have not completed yet.
fn is_job_finished(job: &crate::ui_actions::JobInfo) -> bool {
    is_job_done(job)
        && !job
            .status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("blocked"))
}

/// The selected job when its own status is final for this wait result.
fn finished_wait_job(result: &WaitResult, job_index: i64) -> Option<&crate::ui_actions::JobInfo> {
    find_job(&result.jobs, job_index).filter(|job| {
        is_job_finished(job) || (result.terminal_run_status.is_none() && is_job_done(job))
    })
}

fn wait_target_status(result: &WaitResult, job_index: Option<i64>) -> String {
    let terminal_run_status = result.terminal_run_status.as_deref();
    if let Some(job_index) = job_index {
        if let Some(status) =
            finished_wait_job(result, job_index).and_then(|job| job.status.clone())
        {
            return status;
        }
        let job = find_job(&result.jobs, job_index);
        return match (job, terminal_run_status) {
            // The job snapshot lags behind the terminal run.
            (Some(_), Some(run_status)) => normalize_run_status(run_status),
            _ => job
                .and_then(|job| job.status.clone())
                .unwrap_or_else(|| "missing".to_string()),
        };
    }

    if let Some(run_status) = terminal_run_status {
        return normalize_run_status(run_status);
    }

    for job in &result.jobs {
        let Some(status) = job.status.as_deref() else {
            return "unknown".to_string();
        };
        if is_failing_status(status) {
            return normalize_run_status(status);
        }
    }

    "success".to_string()
}

fn wait_terminal_error(result: &WaitResult, job_index: Option<i64>) -> Option<eyre::Report> {
    let run_index = result.run_index;
    let jobs = &result.jobs;
    let terminal_run_status = result.terminal_run_status.as_deref();
    let Some(job_index) = job_index else {
        return run_terminal_error(run_index, terminal_run_status, jobs);
    };

    if let Some(job) = finished_wait_job(result, job_index) {
        if job.status.as_deref().is_some_and(is_failing_status) {
            let status = crate::output::sanitize_line(job.status.as_deref().unwrap_or("unknown"));
            return Some(eyre!(
                "run {run_index} job {job_index} finished with status {status}"
            ));
        }
        return None;
    }

    // A failed or canceled run explains a missing or stale job best.
    let run_error = terminal_run_status
        .and_then(|run_status| run_terminal_error(run_index, Some(run_status), &[]));
    if find_job(jobs, job_index).is_none() {
        if jobs.is_empty() {
            return run_error.or_else(|| Some(eyre!("run {run_index} returned no jobs")));
        }
        return Some(eyre!("run {run_index} completed without job {job_index}"));
    }

    // The job snapshot is stale; the terminal run status decides.
    run_error
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
    session: &crate::actions_backend::ActionsClient,
    target_base_url: &str,
    repo: &str,
    run_index: i64,
    dry_run: bool,
    json: bool,
) -> eyre::Result<()> {
    session.check_rerun_failed_supported()?;
    let jobs = session.run_jobs(repo, run_index).await?;
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
            for url in session
                .rerun_failed_plan(repo, run_index, &failed_job_indexes)
                .await?
            {
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

    let crate::actions_backend::RerunFailedResult {
        failures,
        redirects,
    } = session
        .rerun_failed(repo, run_index, &failed_job_indexes)
        .await?;

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
            println!("{}", crate::output::sanitize_line(&redirect));
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
    crate::credentials::missing_api_token_error(base_url)
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
    let token = crate::credentials::resolve_api_token(&target.base_url)
        .await?
        .ok_or_else(|| fj_missing_api_token_error(&target.base_url))?;
    let client = crate::api::ApiClient::new_with_socket(
        &target.base_url,
        &token.token,
        target.unix_socket.as_deref(),
    )?;

    match command {
        ActionsRunnersSubcommand::Token { scope, org, json } => {
            let scope = resolve_runner_scope(scope, org.as_deref(), target);
            let (org_out, repo_out, url) =
                resolve_runner_endpoint_url(&client, scope, org, target, "registration-token")?;

            let mut urls = vec![url];
            if scope == RunnerScope::Global {
                // Gitea serves the global token under /admin/actions/runners.
                urls.push(client.api_v1_url("/admin/actions/runners/registration-token"));
            }
            let reg = client.registration_token(&urls).await?;

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
            let mut jobs = client.runner_jobs(&url, &base_jobs_url, &labels).await?;

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
    session: &crate::actions_backend::ActionsClient,
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
        return session.latest_run_index(repo, workflow).await;
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
        "success",
        "failure",
        "running",
        "waiting",
        "cancelling",
        "canceled",
        "skipped",
        "blocked",
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

fn observed_at_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// `logs job --follow` retries a missing log only while the run is still
/// queued or running. A terminal status ends the retries only once it has been
/// stable (`terminal_stable`, see [`observe_terminal_run_status`]).
fn follow_should_wait_for_log(run_status: Option<&str>, terminal_stable: bool) -> bool {
    match run_status_is_terminal(run_status) {
        Some(false) => true,
        Some(true) => !terminal_stable,
        None => false,
    }
}

/// `logs job --follow` on a job index the run does not have fails at once.
fn follow_missing_job_error(
    jobs: &[crate::ui_actions::JobInfo],
    run_index: i64,
    job_index: i64,
) -> Option<eyre::Report> {
    if jobs.is_empty() || find_job(jobs, job_index).is_some() {
        return None;
    }
    Some(eyre!(
        "Run #{run_index} has no job index {job_index} (it has {} job(s)).",
        jobs.len()
    ))
}

fn run_status_is_terminal(status: Option<&str>) -> Option<bool> {
    let status = status?;
    Some(matches!(
        normalize_run_status(status).as_str(),
        "success" | "failure" | "canceled" | "skipped" | "blocked"
    ))
}

/// Forgejo can briefly return a previous terminal run status while a rerun
/// attempt is being scheduled. Require the same terminal status twice before
/// using it to stop a watch or log follow operation.
fn observe_terminal_run_status(
    previous: &mut Option<String>,
    observations: &mut u8,
    status: Option<&str>,
) -> bool {
    let Some(status) = status.filter(|status| run_status_is_terminal(Some(status)) == Some(true))
    else {
        *previous = None;
        *observations = 0;
        return false;
    };
    let status = normalize_run_status(status);
    if previous.as_deref() == Some(status.as_str()) {
        *observations = observations.saturating_add(1);
    } else {
        *previous = Some(status);
        *observations = 1;
    }
    *observations >= 2
}

/// Polls one run's status from the Actions run list, which is authoritative
/// when the run page's per-job snapshot lags behind, and tracks whether a
/// terminal status has been stable (see [`observe_terminal_run_status`]).
struct RunStatusTracker {
    run_index: i64,
    warned_missing: bool,
    terminal_status: Option<String>,
    terminal_observations: u8,
}

impl RunStatusTracker {
    fn new(run_index: i64) -> Self {
        Self {
            run_index,
            warned_missing: false,
            terminal_status: None,
            terminal_observations: 0,
        }
    }

    /// Returns the run-list status, or `None` when the run is not listed (or
    /// has no recognizable status). Callers then fall back to the job snapshot;
    /// a missing run is reported once on stderr.
    async fn poll(
        &mut self,
        session: &crate::actions_backend::ActionsClient,
        repo: &str,
    ) -> eyre::Result<Option<String>> {
        let Some(status) = session.find_run_status(repo, self.run_index).await? else {
            self.warn_missing_once();
            observe_terminal_run_status(
                &mut self.terminal_status,
                &mut self.terminal_observations,
                None,
            );
            return Ok(None);
        };
        observe_terminal_run_status(
            &mut self.terminal_status,
            &mut self.terminal_observations,
            status.as_deref(),
        );
        Ok(status)
    }

    fn warn_missing_once(&mut self) {
        if !std::mem::replace(&mut self.warned_missing, true) {
            eprintln!(
                "warning: run #{} was not found in the Actions run list; falling back to the run page's job status, which may lag behind",
                self.run_index
            );
        }
    }

    /// Whether the run-list status has been terminal for two consecutive polls.
    fn is_terminal(&self) -> bool {
        self.terminal_observations >= 2
    }
}

fn should_print_jobs_snapshot(
    watch: bool,
    done: bool,
    interactive: bool,
    json_lines: bool,
    changed: bool,
) -> bool {
    !watch || done || (interactive && changed) || (json_lines && changed)
}

/// `--json-lines` needs exactly one compact JSON object per line; `--json`
/// keeps the human-friendly pretty layout.
fn format_jobs_snapshot(payload: &serde_json::Value, json_lines: bool) -> eyre::Result<String> {
    let formatted = if json_lines {
        serde_json::to_string(payload)?
    } else {
        serde_json::to_string_pretty(payload)?
    };
    Ok(formatted)
}

fn workflow_filter_for_wait(workflow: &str) -> Option<&str> {
    let workflow = workflow.trim();
    if workflow.chars().all(|ch| ch.is_ascii_digit()) {
        None
    } else {
        Some(workflow)
    }
}

fn is_in_progress_status(status: &str) -> bool {
    IN_PROGRESS_JOB_STATUSES
        .iter()
        .any(|in_progress| status.eq_ignore_ascii_case(in_progress))
}

fn is_failing_status(status: &str) -> bool {
    FAILING_JOB_STATUSES
        .iter()
        .any(|failing| status.eq_ignore_ascii_case(failing))
}

fn is_run_done_from_jobs(jobs: &[crate::ui_actions::JobInfo]) -> bool {
    !jobs
        .iter()
        .any(|j| j.status.as_deref().is_none_or(is_in_progress_status))
}

fn run_terminal_error(
    run_index: i64,
    run_status: Option<&str>,
    jobs: &[crate::ui_actions::JobInfo],
) -> Option<eyre::Report> {
    let run_status = run_status.map(normalize_run_status);
    if matches!(
        run_status.as_deref(),
        Some("failure" | "canceled" | "blocked")
    ) {
        return Some(eyre!(
            "run {run_index} finished with status {}; job snapshot may be stale",
            run_status.unwrap_or_default()
        ));
    }

    // A terminal success from the Actions list is authoritative even when the
    // per-job run-page payload has not caught up yet.
    if matches!(run_status.as_deref(), Some("success" | "skipped")) {
        return None;
    }

    if jobs.is_empty() {
        return Some(eyre!("run {run_index} returned no jobs"));
    }

    let mut failures: Vec<String> = Vec::new();
    for j in jobs {
        let Some(status) = j.status.as_deref() else {
            continue;
        };
        if is_failing_status(status) {
            failures.push(format!(
                "job {}: {}",
                j.job_index,
                crate::output::sanitize_line(status)
            ));
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

#[cfg(test)]
mod tests {
    use super::{
        artifact_items, follow_should_wait_for_log, format_jobs_snapshot, log_delta,
        observe_terminal_run_status, run_status_is_terminal, should_print_jobs_snapshot,
        validate_actions_name,
    };

    #[test]
    fn json_lines_snapshots_are_single_compact_lines() {
        let payload = serde_json::json!({
            "runIndex": 7,
            "jobs": [{"jobIndex": 0, "name": "build\nstep", "status": "running"}],
        });
        let line = format_jobs_snapshot(&payload, true).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            payload
        );

        let pretty = format_jobs_snapshot(&payload, false).unwrap();
        assert!(pretty.contains("\n  \"jobs\""));
    }

    #[test]
    fn terminal_run_status_overrides_a_stale_job_snapshot() {
        assert_eq!(run_status_is_terminal(Some("Failure")), Some(true));
        assert_eq!(run_status_is_terminal(Some("running")), Some(false));
        assert_eq!(run_status_is_terminal(None), None);
    }

    #[test]
    fn follow_log_only_emits_new_bytes_and_detects_replacement() {
        assert_eq!(log_delta(b"one", b"one two"), (b" two".as_slice(), false));
        assert_eq!(log_delta(b"one", b"other"), (b"other".as_slice(), true));
    }

    #[test]
    fn terminal_status_must_be_stable_across_two_observations() {
        let mut previous = None;
        let mut observations = 0;
        assert!(!observe_terminal_run_status(
            &mut previous,
            &mut observations,
            Some("Failure"),
        ));
        assert!(!observe_terminal_run_status(
            &mut previous,
            &mut observations,
            Some("Running"),
        ));
        assert!(!observe_terminal_run_status(
            &mut previous,
            &mut observations,
            Some("Failure"),
        ));
        assert!(observe_terminal_run_status(
            &mut previous,
            &mut observations,
            Some("Failure"),
        ));
    }

    #[test]
    fn artifact_response_is_normalized_to_its_item_list() {
        let response = serde_json::json!({"artifacts": [{"name": "apk"}]});
        assert_eq!(artifact_items(&response).len(), 1);
        assert!(artifact_items(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn json_lines_watch_emits_only_state_changes_and_completion() {
        assert!(should_print_jobs_snapshot(true, false, false, true, true));
        assert!(!should_print_jobs_snapshot(true, false, false, true, false));
        assert!(should_print_jobs_snapshot(true, true, false, true, false));
        assert!(!should_print_jobs_snapshot(true, false, false, false, true));
        assert!(should_print_jobs_snapshot(
            false, false, false, false, false
        ));
    }

    #[test]
    fn actions_value_names_are_validated_before_any_network_request() {
        assert_eq!(
            validate_actions_name("HARBOR_PASSWORD").unwrap(),
            "HARBOR_PASSWORD"
        );
        assert_eq!(validate_actions_name(" _release1 ").unwrap(), "_release1");
        assert!(validate_actions_name("1BAD").is_err());
        assert!(validate_actions_name("HAS-DASH").is_err());
        assert!(validate_actions_name(" ").is_err());
    }

    #[test]
    fn follow_waits_for_logs_only_while_the_run_is_active() {
        assert!(follow_should_wait_for_log(Some("Waiting"), false));
        assert!(follow_should_wait_for_log(Some("Running"), false));
        // A terminal status must be seen twice before it ends the retries.
        assert!(follow_should_wait_for_log(Some("Failure"), false));
        assert!(!follow_should_wait_for_log(Some("Failure"), true));
        assert!(!follow_should_wait_for_log(None, false));
    }

    #[test]
    fn follow_rejects_a_job_index_the_run_does_not_have() {
        let job = |job_index| crate::ui_actions::JobInfo {
            run_index: 7,
            job_index,
            id: None,
            name: None,
            status: Some("running".into()),
            can_rerun: None,
            duration: None,
        };
        let jobs = vec![job(0), job(1)];
        assert!(super::follow_missing_job_error(&jobs, 7, 1).is_none());
        let err = super::follow_missing_job_error(&jobs, 7, 5).unwrap();
        assert!(err.to_string().contains("no job index 5"), "{err}");
        // An empty snapshot is not conclusive.
        assert!(super::follow_missing_job_error(&[], 7, 5).is_none());
    }
}

fn log_delta<'a>(previous: &[u8], current: &'a [u8]) -> (&'a [u8], bool) {
    match current.strip_prefix(previous) {
        Some(delta) => (delta, false),
        None => (current, true),
    }
}

fn artifact_items(artifacts: &serde_json::Value) -> &[serde_json::Value] {
    artifacts
        .get("artifacts")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
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
mod wait_tests {
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

    fn result(
        terminal_run_status: Option<&str>,
        jobs: Vec<crate::ui_actions::JobInfo>,
    ) -> WaitResult {
        WaitResult {
            run_index: 10,
            run_url: "https://forge.example/o/r/actions/runs/10".to_string(),
            terminal_run_status: terminal_run_status.map(str::to_string),
            jobs,
        }
    }

    #[test]
    fn wait_target_done_requires_terminal_statuses() {
        let running = vec![job(0, Some("success")), job(1, Some("running"))];
        assert!(!is_wait_target_done(&running, None, None, false));

        let done = vec![job(0, Some("success")), job(1, Some("skipped"))];
        assert!(is_wait_target_done(&done, None, None, false));
        assert!(is_wait_target_done(&done, Some(0), None, false));
    }

    #[test]
    fn wait_target_done_treats_cancelling_as_in_progress() {
        let jobs = vec![job(0, Some("success")), job(1, Some("cancelling"))];

        assert!(!is_wait_target_done(&jobs, None, None, false));
        assert!(!is_wait_target_done(&jobs, Some(1), None, false));
        assert!(!is_wait_target_done(
            &jobs,
            Some(1),
            Some("Cancelling"),
            false
        ));
        assert!(!is_run_done_from_jobs(&jobs));
        assert_eq!(run_status_is_terminal(Some("Cancelling")), Some(false));
    }

    #[test]
    fn wait_uses_the_run_list_status_over_a_stale_job_snapshot() {
        // The run page still reports a running job, but the run list is terminal.
        let stale = vec![job(0, Some("running"))];
        assert!(is_wait_target_done(&stale, None, Some("Failure"), true));
        assert!(is_wait_target_done(&stale, Some(0), Some("Failure"), true));

        // All jobs look finished, but the run list still reports the run as active.
        let done = vec![job(0, Some("success"))];
        assert!(!is_wait_target_done(&done, None, Some("Running"), false));
        // A first terminal observation is not trusted yet.
        assert!(!is_wait_target_done(&stale, None, Some("Success"), false));

        let failed = result(Some("Failure"), stale.clone());
        assert!(wait_terminal_error(&failed, None).is_some());
        assert!(wait_terminal_error(&failed, Some(0)).is_some());
        assert_eq!(wait_target_status(&failed, None), "failure");
        assert_eq!(wait_target_status(&failed, Some(0)), "failure");

        let succeeded = result(Some("Success"), stale);
        assert!(wait_terminal_error(&succeeded, None).is_none());
        assert!(wait_terminal_error(&succeeded, Some(0)).is_none());
        assert_eq!(wait_target_status(&succeeded, None), "success");
    }

    #[test]
    fn job_wait_ends_when_the_job_finishes_before_its_run() {
        let jobs = vec![job(0, Some("success")), job(1, Some("running"))];
        assert!(is_wait_target_done(&jobs, Some(0), Some("Running"), false));
        assert!(!is_wait_target_done(&jobs, Some(1), Some("Running"), false));
    }

    #[test]
    fn job_wait_keeps_waiting_for_a_job_blocked_on_its_needs() {
        let jobs = vec![job(0, Some("running")), job(1, Some("blocked"))];
        assert!(!is_wait_target_done(&jobs, Some(1), Some("Running"), false));

        // A run that stays blocked (e.g. awaiting approval) still ends the wait.
        assert!(is_wait_target_done(&jobs, Some(1), Some("Blocked"), true));
        let blocked = result(Some("Blocked"), jobs);
        assert!(wait_terminal_error(&blocked, Some(1)).is_some());
    }

    #[test]
    fn wait_terminal_error_reports_failed_run_or_job() {
        let jobs = vec![job(0, Some("success")), job(1, Some("failure"))];
        let snapshot = result(None, jobs.clone());
        assert!(wait_terminal_error(&snapshot, None).is_some());
        assert!(wait_terminal_error(&snapshot, Some(1)).is_some());
        assert!(wait_terminal_error(&snapshot, Some(0)).is_none());
        assert!(wait_terminal_error(&snapshot, Some(5)).is_some());
        assert!(wait_terminal_error(&result(None, Vec::new()), Some(0)).is_some());
        assert!(wait_terminal_error(&result(Some("Failure"), jobs), Some(0)).is_none());
    }

    #[test]
    fn workflow_filter_for_wait_omits_numeric_dispatch_ids() {
        assert_eq!(workflow_filter_for_wait("12345"), None);
        assert_eq!(workflow_filter_for_wait("ci.yml"), Some("ci.yml"));
    }

    fn run(run_index: i64, branch: Option<&str>) -> crate::ui_actions::RunRef {
        crate::ui_actions::RunRef {
            run_index,
            url: format!("https://forge.example/o/r/actions/runs/{run_index}"),
            status: Some("Waiting".to_string()),
            branch: branch.map(str::to_string),
            created_at: None,
        }
    }

    #[test]
    fn dispatched_run_must_be_newer_than_the_pre_dispatch_latest() {
        let runs = vec![run(41, Some("main")), run(40, Some("main"))];
        assert_eq!(
            select_dispatched_run(&runs, Some(40), "refs/heads/main"),
            Some(41)
        );
        assert_eq!(
            select_dispatched_run(&runs, Some(41), "refs/heads/main"),
            None
        );
        assert_eq!(
            select_dispatched_run(&runs, None, "refs/heads/main"),
            Some(40)
        );
    }

    #[test]
    fn numeric_workflow_wait_ignores_concurrent_runs_for_other_refs() {
        // `--workflow 12345` cannot filter the run list, so concurrent runs of
        // other workflows are candidates too; the dispatched ref narrows them.
        assert_eq!(workflow_filter_for_wait("12345"), None);
        let runs = vec![
            run(52, Some("main")),
            run(51, Some("feature/x")),
            run(50, Some("main")),
        ];
        assert_eq!(
            select_dispatched_run(&runs, Some(50), "refs/heads/feature/x"),
            Some(51)
        );
        // Of several matching new runs, the oldest one is the dispatched run.
        assert_eq!(
            select_dispatched_run(&runs, Some(50), "refs/heads/main"),
            Some(52)
        );
        let concurrent = vec![run(53, Some("main")), run(52, Some("main"))];
        assert_eq!(
            select_dispatched_run(&concurrent, Some(51), "main"),
            Some(52)
        );
    }

    #[test]
    fn dispatch_ref_matching_accepts_short_labels_and_unlabeled_runs() {
        assert!(run_ref_matches_dispatch(Some("main"), "refs/heads/main"));
        assert!(run_ref_matches_dispatch(Some("v1.2"), "refs/tags/v1.2"));
        assert!(run_ref_matches_dispatch(
            Some("refs/heads/main"),
            "refs/heads/main"
        ));
        assert!(run_ref_matches_dispatch(None, "refs/heads/main"));
        assert!(!run_ref_matches_dispatch(Some("dev"), "refs/heads/main"));
    }
}
