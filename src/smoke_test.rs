use crate::cli::SmokeTestCommand;
use tokio::io::AsyncWriteExt;

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
    let session = crate::session::UiSession::from_store_with_socket(
        &target.base_url,
        false,
        target.unix_socket.as_deref(),
    )
    .await?;

    println!("[2] Workflows");
    let workflows = crate::ui_actions::list_workflows(&session, &repo, 1, 20).await?;
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

    let base_out_dir = args
        .opts
        .out_dir
        .unwrap_or_else(|| std::env::temp_dir().join("fj-ex").join("forgejo-logs"));
    let out_dir = base_out_dir.join(format!("smoketest-run-{latest_run_index}"));
    tokio::fs::create_dir_all(&out_dir).await?;
    let out_file = out_dir.join(format!(
        "job-{}-attempt-{}.log",
        job0.job_index, meta.attempt_number
    ));

    let resp = crate::ui_actions::open_job_logs(
        &session,
        &repo,
        latest_run_index,
        job0.job_index,
        meta.attempt_number,
    )
    .await?;
    let mut file = tokio::fs::File::create(&out_file).await?;
    let log_bytes = crate::ui_actions::copy_response_body_with_limit(
        resp,
        &mut file,
        args.opts.log_download_max_bytes,
    )
    .await
    .map_err(|err| {
        eyre::eyre!(
            "Log file exceeds LogDownloadMaxBytes ({}): {err}. Rerun with a larger limit.",
            args.opts.log_download_max_bytes
        )
    })?;
    file.flush().await?;
    if log_bytes == 0 {
        return Err(eyre::eyre!("Log file is empty (expected non-empty)"));
    }

    let len = tokio::fs::metadata(&out_file).await?.len();
    println!("logBytes={len} outFile={}", out_file.display());

    println!("[7] Non-destructive command checks");
    let repo_path = repo.trim_matches('/');
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
