use std::borrow::Cow;
use std::io::Read;
use std::path::{Path, PathBuf};

use eyre::{eyre, Context};
use reqwest::{RequestBuilder, Response};
use serde_json::{json, Value};

use crate::cli::{PullsCommand, PullsSubcommand};
use crate::output::{sanitize_line, sanitize_terminal_text};

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
        PullsSubcommand::List {
            state,
            page,
            limit,
            json,
        } => {
            let url = format!(
                "{api_root}/pulls?state={}&page={page}&limit={limit}",
                state.as_str()
            );
            let response =
                send_authenticated(client.get(url), &credentials, "pull request list").await?;
            let pulls = response_value(response, "pull request list").await?;
            let output = json!({
                "repo": repo,
                "state": state.as_str(),
                "page": page,
                "limit": limit,
                "pulls": pulls,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_pull_list(&output["pulls"])?;
            }
        }
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
            println!("Created pull request #{number} {}", sanitize_line(html_url));
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
                let detail = read_error_detail(response).await;
                if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
                    return Err(eyre!(
                        "Pull request merge rejected: HTTP {status}. Forgejo uses 405 when the caller cannot merge the current PR state. \
                         Authentication source: {}. Check the caller has repository admin/merge permission and that branch-protection approvals/checks are satisfied; \
                         use --force only with explicit operator authorization.{}",
                        credentials.source_label(),
                        detail,
                    ));
                }
                return Err(eyre!(
                    "Pull request merge failed: HTTP {status}. Authentication source: {}.{}",
                    credentials.source_label(),
                    detail,
                ));
            }
            println!("Merged pull request #{index}.");
        }
        PullsSubcommand::Comment {
            index,
            body,
            body_file,
            json,
        } => {
            let body = read_body(body, body_file)?;
            let response = send_authenticated(
                client
                    .post(format!("{api_root}/issues/{index}/comments"))
                    .json(&json!({ "body": body })),
                &credentials,
                "pull request comment",
            )
            .await?;
            let comment = response_value(response, "pull request comment").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&comment)?);
            } else {
                let id = comment
                    .get("id")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                println!("Posted comment #{id} on pull request #{index}.");
            }
        }
        PullsSubcommand::Review {
            index,
            body,
            body_file,
            event,
            commit,
            json,
        } => {
            let body = read_body(body, body_file)?;
            let mut request_body = json!({
                "body": body,
                "event": event.as_api_value(),
            });
            if let Some(commit) = commit {
                request_body["commit_id"] = Value::String(nonempty("--commit", commit)?);
            }
            let response = send_authenticated(
                client
                    .post(format!("{api_root}/pulls/{index}/reviews"))
                    .json(&request_body),
                &credentials,
                "pull request review",
            )
            .await?;
            let review = response_value(response, "pull request review").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&review)?);
            } else {
                let id = review.get("id").and_then(Value::as_u64).unwrap_or_default();
                println!("Posted {event:?} review #{id} on pull request #{index}.");
            }
        }
        PullsSubcommand::Comments {
            index,
            page,
            limit,
            json,
        } => {
            let url = format!("{api_root}/issues/{index}/comments?page={page}&limit={limit}");
            let response =
                send_authenticated(client.get(url), &credentials, "pull request comments").await?;
            let comments = response_value(response, "pull request comments").await?;
            let output = json!({
                "repo": repo,
                "index": index,
                "page": page,
                "limit": limit,
                "comments": comments,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_comment_list(&output["comments"])?;
            }
        }
        PullsSubcommand::Reviews {
            index,
            page,
            limit,
            json,
        } => {
            let url = format!("{api_root}/pulls/{index}/reviews?page={page}&limit={limit}");
            let response =
                send_authenticated(client.get(url), &credentials, "pull request reviews").await?;
            let reviews = response_value(response, "pull request reviews").await?;
            let output = json!({
                "repo": repo,
                "index": index,
                "page": page,
                "limit": limit,
                "reviews": reviews,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_review_list(&output["reviews"])?;
            }
        }
    }

    Ok(())
}

type ApiCredentials = crate::credentials::ApiAuth;

async fn api_credentials(target: &crate::target::ResolvedTarget) -> eyre::Result<ApiCredentials> {
    // API tokens carry the caller's repository permissions. Prefer them over
    // UI credentials so PR administration is not silently performed as a
    // weaker automation account. Token sources: `fj-ex auth login --token-stdin`,
    // FJ_EX_TOKEN / FORGEJO_TOKEN / GITEA_TOKEN, then `fj`'s keys.json.
    crate::credentials::resolve_api_auth(&target.base_url)
        .await?
        .ok_or_else(|| crate::credentials::missing_api_auth_error(&target.base_url))
}

fn authenticate_request(
    request: reqwest::RequestBuilder,
    credentials: &ApiCredentials,
) -> eyre::Result<reqwest::RequestBuilder> {
    credentials.apply(request)
}

/// Server error details shown to the user are clipped to this many characters.
const ERROR_DETAIL_MAX_CHARS: usize = 1_000;
/// Bytes of an error body kept in memory: enough for the clipped detail even
/// if every character takes four UTF-8 bytes, plus one byte to notice more.
const ERROR_BODY_MAX_BYTES: usize = ERROR_DETAIL_MAX_CHARS * 4 + 1;

/// Formats a (possibly already truncated) server error body for an error
/// message. `truncated` marks a body that was cut off while reading.
fn response_detail(detail: &str, truncated: bool) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        return String::new();
    }
    // Error messages end up on a terminal as well.
    let detail = sanitize_terminal_text(detail, true);
    let mut clipped = detail
        .chars()
        .take(ERROR_DETAIL_MAX_CHARS)
        .collect::<String>();
    if truncated || detail.chars().count() > ERROR_DETAIL_MAX_CHARS {
        clipped.push_str("...");
    }
    format!(" Response: {clipped}")
}

/// Accumulates at most `max_bytes` of a response body, dropping the rest.
struct BodyPrefix {
    bytes: Vec<u8>,
    max_bytes: usize,
    truncated: bool,
}

impl BodyPrefix {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            truncated: false,
        }
    }

    /// Appends a chunk; returns `false` once the prefix is full.
    fn push(&mut self, chunk: &[u8]) -> bool {
        let room = self.max_bytes.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.bytes.extend_from_slice(&chunk[..room]);
            self.truncated = true;
            return false;
        }
        self.bytes.extend_from_slice(chunk);
        true
    }

    /// Decodes the prefix, dropping a multi-byte character cut in half at the end.
    fn into_detail(self) -> String {
        let end = complete_utf8_prefix_len(&self.bytes);
        let text = String::from_utf8_lossy(&self.bytes[..end]);
        response_detail(&text, self.truncated)
    }
}

/// Length of `bytes` without a trailing, incomplete UTF-8 sequence.
fn complete_utf8_prefix_len(bytes: &[u8]) -> usize {
    // A UTF-8 sequence is at most four bytes, so only the last three can
    // belong to an unfinished character.
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue; // continuation byte
        }
        let needed = match byte {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        };
        return if needed > back {
            bytes.len() - back
        } else {
            bytes.len()
        };
    }
    bytes.len()
}

/// Reads an error response body incrementally and returns its clipped detail
/// without buffering an arbitrarily large body.
pub(crate) async fn read_error_detail(mut response: Response) -> String {
    let mut prefix = BodyPrefix::new(ERROR_BODY_MAX_BYTES);
    // A body that fails mid-read still yields whatever arrived before.
    while let Ok(Some(chunk)) = response.chunk().await {
        if !prefix.push(&chunk) {
            break;
        }
    }
    prefix.into_detail()
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

async fn send_authenticated(
    request: RequestBuilder,
    credentials: &ApiCredentials,
    operation: &str,
) -> eyre::Result<Response> {
    authenticate_request(request, credentials)?
        .send()
        .await
        .wrap_err_with(|| format!("{operation} request failed"))
}

async fn response_value(response: Response, operation: &str) -> eyre::Result<Value> {
    let status = response.status();
    if !status.is_success() {
        let detail = read_error_detail(response).await;
        return Err(eyre!("{operation} failed: HTTP {status}.{detail}"));
    }
    let body = response
        .bytes()
        .await
        .wrap_err_with(|| format!("failed to read {operation} response"))?;
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&body)
        .wrap_err_with(|| format!("failed to parse {operation} response as JSON"))
}

fn read_body(body: Option<String>, body_file: Option<PathBuf>) -> eyre::Result<String> {
    let value = match (body, body_file) {
        (Some(body), None) => body,
        (None, Some(path)) if path == Path::new("-") => {
            let mut body = String::new();
            std::io::stdin()
                .read_to_string(&mut body)
                .wrap_err("failed to read comment body from stdin")?;
            body
        }
        (None, Some(path)) => std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read comment body file '{}'", path.display()))?,
        (Some(_), Some(_)) => unreachable!("clap prevents conflicting body sources"),
        (None, None) => return Err(eyre!("provide either --body or --body-file")),
    };
    nonempty("comment body", value)
}

// Human-readable list output. Every server-controlled string is sanitized so
// it cannot inject terminal escape sequences; `--json` output stays verbatim.

fn print_pull_list(value: &Value) -> eyre::Result<()> {
    print_lines(value, "pull request list", format_pull_line)
}

fn print_comment_list(value: &Value) -> eyre::Result<()> {
    print_lines(value, "comment list", format_comment_line)
}

fn print_review_list(value: &Value) -> eyre::Result<()> {
    print_lines(value, "review list", format_review_line)
}

fn print_lines(value: &Value, what: &str, format: fn(&Value) -> String) -> eyre::Result<()> {
    for item in value
        .as_array()
        .ok_or_else(|| eyre!("{what} response was not an array"))?
    {
        println!("{}", format(item));
    }
    Ok(())
}

fn str_field<'a>(value: &'a Value, pointer: &str, default: &'a str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or(default)
}

fn line_field<'a>(value: &'a Value, pointer: &str, default: &'a str) -> Cow<'a, str> {
    sanitize_line(str_field(value, pointer, default))
}

fn body_field<'a>(value: &'a Value) -> Cow<'a, str> {
    sanitize_terminal_text(str_field(value, "/body", ""), true)
}

fn format_pull_line(pull: &Value) -> String {
    let number = pull
        .get("number")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let state = line_field(pull, "/state", "?");
    let title = line_field(pull, "/title", "");
    let head = line_field(pull, "/head/ref", "");
    let base = line_field(pull, "/base/ref", "");
    format!("#{number}\t{state}\t{head} -> {base}\t{title}")
}

fn format_comment_line(comment: &Value) -> String {
    let id = comment
        .get("id")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let user = line_field(comment, "/user/login", "?");
    let body = body_field(comment);
    format!("#{id}\t{user}\t{body}")
}

fn format_review_line(review: &Value) -> String {
    let id = review.get("id").and_then(Value::as_u64).unwrap_or_default();
    let user = line_field(review, "/user/login", "?");
    let state = line_field(review, "/state", "?");
    let body = body_field(review);
    format!("#{id}\t{user}\t{state}\t{body}")
}

#[cfg(test)]
mod tests {
    use super::{
        complete_utf8_prefix_len, format_comment_line, format_pull_line, format_review_line,
        nonempty, read_body, request_base, response_detail, BodyPrefix, ERROR_BODY_MAX_BYTES,
        ERROR_DETAIL_MAX_CHARS,
    };
    use crate::cli::{PullState, ReviewEvent};

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
        assert_eq!(
            response_detail(" no approval\n", false),
            " Response: no approval"
        );
        assert!(response_detail(&"x".repeat(1_001), false).ends_with("..."));
        assert_eq!(response_detail("short", true), " Response: short...");
        assert_eq!(response_detail("  ", false), "");
    }

    #[test]
    fn human_output_neutralizes_terminal_escapes_in_server_fields() {
        let pull = serde_json::json!({
            "number": 7,
            "state": "open\u{1b}[2J",
            "title": "fix\u{1b}]8;;https://evil\u{07}title\tpart",
            "head": {"ref": "feat\u{9b}31m"},
            "base": {"ref": "main\r"},
        });
        let line = format_pull_line(&pull);
        assert!(!line.chars().any(|ch| ch.is_control() && ch != '\t'));
        assert_eq!(
            line,
            "#7\topen\u{fffd}[2J\tfeat\u{fffd}31m -> main \tfix\u{fffd}]8;;https://evil\u{fffd}title part"
        );

        let comment = serde_json::json!({
            "id": 3,
            "user": {"login": "mallory\u{1b}[8m"},
            "body": "line 1\r\n\tline 2\u{1b}[0m",
        });
        assert_eq!(
            format_comment_line(&comment),
            "#3\tmallory\u{fffd}[8m\tline 1\n\tline 2\u{fffd}[0m"
        );

        let review = serde_json::json!({
            "id": 4,
            "user": {"login": "bob"},
            "state": "APPROVED\u{7}",
            "body": "ok\u{1b}c",
        });
        assert_eq!(
            format_review_line(&review),
            "#4\tbob\tAPPROVED\u{fffd}\tok\u{fffd}c"
        );
        assert_eq!(
            response_detail("{\"message\":\"\u{1b}[31mdenied\"}", false),
            " Response: {\"message\":\"\u{fffd}[31mdenied\"}"
        );
    }

    #[test]
    fn error_body_prefix_keeps_only_a_bounded_prefix_of_an_oversized_body() {
        let mut prefix = BodyPrefix::new(ERROR_BODY_MAX_BYTES);
        let chunk = "é".repeat(1_024); // 2 bytes per character
        let mut chunks_accepted = 0;
        for _ in 0..1_000 {
            if !prefix.push(chunk.as_bytes()) {
                break;
            }
            chunks_accepted += 1;
        }
        assert!(chunks_accepted < 1_000, "reading must stop once full");
        assert_eq!(prefix.bytes.len(), ERROR_BODY_MAX_BYTES);
        assert!(prefix.truncated);

        let detail = prefix.into_detail();
        let clipped = detail.strip_prefix(" Response: ").unwrap();
        let clipped = clipped.strip_suffix("...").unwrap();
        assert_eq!(clipped.chars().count(), ERROR_DETAIL_MAX_CHARS);
        assert!(clipped.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn error_body_prefix_drops_a_character_split_at_the_byte_limit() {
        let mut prefix = BodyPrefix::new(4);
        assert!(!prefix.push("ab€".as_bytes())); // '€' is three bytes
        assert_eq!(prefix.into_detail(), " Response: ab...");

        assert_eq!(complete_utf8_prefix_len("a€".as_bytes()), 4);
        assert_eq!(complete_utf8_prefix_len(&"a€".as_bytes()[..3]), 1);
        assert_eq!(complete_utf8_prefix_len(&"😀".as_bytes()[..3]), 0);
        assert_eq!(complete_utf8_prefix_len("😀".as_bytes()), 4);
        assert_eq!(complete_utf8_prefix_len(b""), 0);
    }

    #[test]
    fn uses_explicit_comment_body_sources() {
        assert_eq!(read_body(Some("hello".to_string()), None).unwrap(), "hello");
        assert!(read_body(None, None).is_err());
        assert!(read_body(Some(" ".to_string()), None).is_err());
    }

    #[test]
    fn serializes_safe_pull_filters_and_review_events() {
        assert_eq!(PullState::Open.as_str(), "open");
        assert_eq!(PullState::All.as_str(), "all");
        assert_eq!(ReviewEvent::Comment.as_api_value(), "COMMENT");
        assert_eq!(
            ReviewEvent::RequestChanges.as_api_value(),
            "REQUEST_CHANGES"
        );
    }
}
