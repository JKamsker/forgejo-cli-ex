use eyre::{eyre, Context};

use crate::cli::{PullsCommand, PullsSubcommand};

pub async fn run(args: PullsCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let repo = target
        .repo
        .as_deref()
        .ok_or_else(|| eyre!("Pass --repo owner/name or run inside a repository with origin."))?;
    let (owner, name) = repo.trim_matches('/').split_once('/').ok_or_else(|| {
        eyre!(
            "Repo should be in the format owner/name; got '{}'. Pass --repo owner/name.",
            repo
        )
    })?;
    let creds = api_credentials(&target).await?;
    let client = api_client(&target)?;
    let base = request_base(&target.base_url);
    let api_root = format!(
        "{}/api/v1/repos/{}/{}",
        base,
        urlencoding::encode(owner),
        urlencoding::encode(name)
    );

    match args.command {
        PullsSubcommand::Create {
            head,
            base,
            title,
            body,
        } => {
            let head = nonempty("--head", head)?;
            let base = nonempty("--base", base)?;
            let title = nonempty("--title", title)?;
            let response = client
                .post(format!("{api_root}/pulls"))
                .basic_auth(&creds.username, Some(&creds.password))
                .json(&serde_json::json!({
                    "head": head,
                    "base": base,
                    "title": title,
                    "body": body.unwrap_or_default(),
                }))
                .send()
                .await
                .wrap_err("pull request creation request failed")?;
            if response.status() != reqwest::StatusCode::CREATED {
                return Err(eyre!(
                    "Pull request creation failed: HTTP {}",
                    response.status()
                ));
            }
            let created: serde_json::Value = response
                .json()
                .await
                .wrap_err("failed to parse created pull request")?;
            let number = created
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| eyre!("created pull request response had no number"))?;
            let html_url = created
                .get("html_url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            println!("Created pull request #{number} {html_url}");
        }
        PullsSubcommand::Merge {
            index,
            head_commit,
            title,
            force,
        } => {
            if index == 0 {
                return Err(eyre!("--index must be greater than zero"));
            }
            let head_commit = nonempty("--head-commit", head_commit)?;
            let title = nonempty("--title", title)?;
            let response = client
                .post(format!("{api_root}/pulls/{index}/merge"))
                .basic_auth(&creds.username, Some(&creds.password))
                .json(&serde_json::json!({
                    "Do": "merge",
                    "head_commit_id": head_commit,
                    "MergeTitleField": title,
                    "MergeMessageField": "",
                    "force_merge": force,
                    "merge_when_checks_succeed": false,
                }))
                .send()
                .await
                .wrap_err("pull request merge request failed")?;
            if response.status() != reqwest::StatusCode::OK {
                return Err(eyre!(
                    "Pull request merge failed: HTTP {}",
                    response.status()
                ));
            }
            println!("Merged pull request #{index}.");
        }
    }

    Ok(())
}

async fn api_credentials(
    target: &crate::target::ResolvedTarget,
) -> eyre::Result<crate::store::UiCreds> {
    crate::store::get_ui_creds(&target.base_url)
        .await?
        .ok_or_else(|| {
            eyre!(
                "No stored UI creds for '{}'. Run `fj-ex auth login` first.",
                target.base_url
            )
        })
}

fn api_client(target: &crate::target::ResolvedTarget) -> eyre::Result<reqwest::Client> {
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
    builder.build().wrap_err("failed to build HTTP client")
}

fn request_base(base_url: &str) -> &str {
    if base_url.starts_with("http+unix://") {
        "http://localhost"
    } else {
        base_url.trim_end_matches('/')
    }
}

fn nonempty(flag: &str, value: String) -> eyre::Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(eyre!("{flag} must not be empty"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{nonempty, request_base};

    #[test]
    fn normalizes_pull_request_inputs_without_changing_content() {
        assert_eq!(nonempty("--title", " title ".to_string()).unwrap(), "title");
        assert!(nonempty("--base", " ".to_string()).is_err());
        assert_eq!(
            request_base("https://forge.example/"),
            "https://forge.example"
        );
        assert_eq!(
            request_base("http+unix://%2Ftmp%2Fforgejo.sock"),
            "http://localhost"
        );
    }
}
