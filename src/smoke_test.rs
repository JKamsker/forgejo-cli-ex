use crate::cli::SmokeTestCommand;

pub async fn run(args: SmokeTestCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;

    let repo = target.repo.clone().ok_or_else(|| {
        eyre::eyre!(
            "Repo could not be resolved. Pass --repo owner/name or run inside a git repo with a Forgejo remote."
        )
    })?;

    println!("Forgejo: {}", target.base_url);
    println!("Repo:    {}", repo);

    println!();
    println!("[1] Session from store (cookie jar preferred)");
    let session = crate::session::UiSession::from_store(&target.base_url, false).await?;

    println!("[2] Workflows");
    let workflows = crate::ui_actions::list_workflows(&session, &repo, 1).await?;
    println!("workflows={}", workflows.len());

    println!("[3] Runs (page 1, limit 5)");
    let runs = crate::ui_actions::list_runs(&session, &repo, None, 1, 5).await?;
    println!("runs={}", runs.len());
    if runs.is_empty() {
        return Err(eyre::eyre!("At least one run exists"));
    }
    let latest_run_index = runs[0].run_index;
    println!("latestRunIndex={latest_run_index}");

    println!("[4] Jobs for latest run");
    let view = crate::ui_actions::get_run_view_data(&session, &repo, latest_run_index).await?;
    let jobs = crate::ui_actions::get_run_jobs(latest_run_index, &view.view)?;
    println!("jobs={}", jobs.len());
    if jobs.is_empty() {
        return Err(eyre::eyre!("At least one job exists"));
    }
    let job0 = &jobs[0];
    println!(
        "job0Index={} job0Status={} job0Name={}",
        job0.job_index,
        job0.status.as_deref().unwrap_or("?"),
        job0.name.as_deref().unwrap_or("?")
    );

    println!("[5] Artifacts for latest run");
    let artifacts = crate::ui_actions::get_run_artifacts(&session, &repo, latest_run_index).await?;
    let artifact_count = artifacts
        .get("artifacts")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    println!("artifactCount={artifact_count}");

    println!("[6] Job attempt metadata + log download");
    let meta =
        crate::ui_actions::get_job_view_meta(&session, &repo, latest_run_index, job0.job_index)
            .await?;
    println!("attempt={}", meta.attempt_number);

    let out_dir = std::path::PathBuf::from(".tmp")
        .join("forgejo-logs")
        .join(format!("smoketest-run-{latest_run_index}"));
    tokio::fs::create_dir_all(&out_dir).await?;
    let out_file = out_dir.join(format!(
        "job-{}-attempt-{}.log",
        job0.job_index, meta.attempt_number
    ));

    let repo_path = repo.trim_matches('/');
    let logs_url = format!(
        "{}/{repo_path}/actions/runs/{latest_run_index}/jobs/{}/attempt/{}/logs",
        session.base_url(),
        job0.job_index,
        meta.attempt_number
    );
    let resp = session.get_response(&logs_url, true).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(eyre::eyre!(
            "Failed to download logs from '{}'. HTTP {}.",
            logs_url,
            resp.status()
        ));
    }
    let bytes = resp.bytes().await?;
    if bytes.is_empty() {
        return Err(eyre::eyre!("Log file non-empty"));
    }
    if bytes.len() as u64 > args.log_download_max_bytes {
        return Err(eyre::eyre!(
            "Log file <= LogDownloadMaxBytes ({}). If this fails, rerun with a larger limit.",
            args.log_download_max_bytes
        ));
    }
    tokio::fs::write(&out_file, bytes).await?;

    let len = tokio::fs::metadata(&out_file).await?.len();
    println!("logBytes={len} outFile={}", out_file.display());

    println!("[7] Non-destructive command checks");
    let cancel_url = format!(
        "{}/{repo_path}/actions/runs/{latest_run_index}/cancel",
        session.base_url()
    );
    let rerun_url = format!(
        "{}/{repo_path}/actions/runs/{latest_run_index}/rerun",
        session.base_url()
    );
    println!("cancelDryRun=POST {cancel_url}");
    println!("rerunDryRun=POST {rerun_url}");

    println!();
    println!("OK");
    Ok(())
}
