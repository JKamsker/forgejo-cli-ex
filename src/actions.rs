use eyre::{eyre, Context};

use crate::cli::{ActionsCommand, ActionsSubcommand};

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
            let runs = crate::ui_actions::list_runs(
                &session,
                &repo,
                workflow.as_deref(),
                page,
                limit,
            )
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
        _ => {
            return Err(eyre!("not implemented yet"));
        }
    }

    Ok(())
}
