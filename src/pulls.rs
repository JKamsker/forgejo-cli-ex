use eyre::{eyre, Context};
use reqwest::header::{HeaderValue, AUTHORIZATION};

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
    let credentials = api_credentials(&target).await?;
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
                .json(&serde_json::json!({
                    "head": head,
                    "base": base,
                    "title": title,
                    "body": body.unwrap_or_default(),
                }));
            let response = authenticate_request(response, &credentials)?
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
            let response =
                client
                    .post(format!("{api_root}/pulls/{index}/merge"))
                    .json(&serde_json::json!({
                        "Do": "merge",
                        "head_commit_id": head_commit,
                        "MergeTitleField": title,
                        "MergeMessageField": "",
                        "force_merge": force,
                        "merge_when_checks_succeed": false,
                    }));
            let response = authenticate_request(response, &credentials)?
                .send()
                .await
                .wrap_err("pull request merge request failed")?;
            if response.status() != reqwest::StatusCode::OK {
                let status = response.status();
                let detail = response.text().await.unwrap_or_default();
                let detail = detail.trim();
                if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
                    return Err(eyre!(
                        "Pull request merge rejected: HTTP {status}. Forgejo uses 405 when the caller cannot merge the current PR state. \
                         Authentication source: {}. Check the caller has repository admin/merge permission and that branch-protection approvals/checks are satisfied; \
                         use --force only with explicit operator authorization.{}",
                        credentials.source(),
                        response_detail(detail),
                    ));
                }
                return Err(eyre!(
                    "Pull request merge failed: HTTP {status}. Authentication source: {}.{}",
                    credentials.source(),
                    response_detail(detail),
                ));
            }
            println!("Merged pull request #{index}.");
        }
    }

    Ok(())
}

enum ApiCredentials {
    ApiToken(String),
    Basic(crate::store::UiCreds),
}

impl ApiCredentials {
    fn source(&self) -> &'static str {
        match self {
            Self::ApiToken(_) => "fj API token",
            Self::Basic(_) => "stored UI basic credentials",
        }
    }
}

async fn api_credentials(target: &crate::target::ResolvedTarget) -> eyre::Result<ApiCredentials> {
    // `fj` API tokens carry the caller's Forgejo repository permissions. Prefer
    // them over UI credentials so PR administration is not silently performed
    // as a weaker automation account.
    if let Some(token) = crate::store::get_fj_api_token_for_base_url(&target.base_url)? {
        return Ok(ApiCredentials::ApiToken(token));
    }

    crate::store::get_ui_creds(&target.base_url)
        .await?
        .map(ApiCredentials::Basic)
        .ok_or_else(|| {
            eyre!(
                "No API token or stored UI creds for '{}'. Configure `fj auth login` for an API token, or run `fj-ex auth login` first.",
                target.base_url
            )
        })
}

fn authenticate_request(
    request: reqwest::RequestBuilder,
    credentials: &ApiCredentials,
) -> eyre::Result<reqwest::RequestBuilder> {
    match credentials {
        ApiCredentials::ApiToken(token) => {
            let value = HeaderValue::from_str(&format!("token {token}"))
                .wrap_err("invalid fj API token for Authorization header")?;
            Ok(request.header(AUTHORIZATION, value))
        }
        ApiCredentials::Basic(creds) => {
            Ok(request.basic_auth(&creds.username, Some(&creds.password)))
        }
    }
}

fn response_detail(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        let mut clipped = detail.chars().take(1_000).collect::<String>();
        if detail.chars().count() > clipped.chars().count() {
            clipped.push_str("...");
        }
        format!(" Response: {clipped}")
    }
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
    use super::{nonempty, request_base, response_detail};

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

    #[test]
    fn limits_server_error_detail_without_hiding_a_concise_message() {
        assert_eq!(response_detail("no approval"), " Response: no approval");
        assert!(response_detail(&"x".repeat(1_001)).ends_with("..."));
    }
}
