use std::io::IsTerminal;

use eyre::{eyre, Context};

use crate::cli::LoginCommand;

struct LoginInput {
    username: String,
    password: String,
    otp: Option<String>,
}

/// Default session cookie names tried for a bare `--session-cookie-stdin` value:
/// recent Forgejo releases (verified 15-17) use `session`, Gitea and older Forgejo
/// releases (verified Forgejo 13) use `i_like_gitea`; `i_like_forgejo` is a common
/// custom `[session] COOKIE_NAME`.
const DEFAULT_SESSION_COOKIE_NAMES: [&str; 3] = ["session", "i_like_gitea", "i_like_forgejo"];

pub async fn run(args: LoginCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;

    if args.token.is_some() || args.token_stdin {
        return run_token_login(args, &target).await;
    }
    if args.session_cookie_stdin {
        return run_session_import(args, &target).await;
    }

    let base_url = target.base_url;

    let input = resolve_login_input(args).await?;

    // Validate login by actually logging in via UI.
    let session =
        crate::session::UiSession::new_with_socket(&base_url, None, target.unix_socket.as_deref())?;
    match session
        .login_with_creds_and_otp(&input.username, &input.password, input.otp.as_deref())
        .await?
    {
        crate::session::LoginResult::LoggedIn => {}
        crate::session::LoginResult::TwoFactorRequired => {
            let passcode = prompt_password("Forgejo 2FA passcode")
                .await
                .map_err(two_factor_prompt_error)?;
            session.submit_two_factor_code(&passcode).await?;
        }
    }

    // Persist plaintext creds (required by design).
    crate::store::set_ui_creds(&base_url, &input.username, &input.password).await?;

    // Persist cookies.
    session.persist_cookie_jar_required().await?;

    let store_path = crate::store::ui_creds_store_paths()?.path;

    println!(
        "{}@{}",
        crate::output::sanitize_line(&input.username),
        host_label(&base_url)
    );
    println!("Saved UI creds to: {}", store_path.display());

    Ok(())
}

fn host_label(base_url: &str) -> String {
    url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| base_url.to_string())
}

async fn run_token_login(
    args: LoginCommand,
    target: &crate::target::ResolvedTarget,
) -> eyre::Result<()> {
    let base_url = &target.base_url;
    let token = match args.token {
        Some(token) => {
            eprintln!(
                "warn: --token exposes the token in the process list and shell history; prefer --token-stdin."
            );
            token
        }
        None => read_secret_from_stdin("Forgejo API token").await?,
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(eyre!("API token must not be empty."));
    }

    let validation = validate_token(base_url, &token, target.unix_socket.as_deref()).await?;
    let login = match &validation {
        TokenValidation::User(login) => Some(login.clone()),
        TokenValidation::MissingUserScope => {
            eprintln!(
                "warn: the token was accepted but lacks the `read:user` scope, so the username is unknown. \
                 Actions commands only need repository scopes; add `read:user` for `auth status` to show the user."
            );
            None
        }
    };

    let outcome = crate::store::set_api_token(base_url, &token, login.as_deref()).await?;
    let store_path = crate::store::ui_creds_store_paths()?.path;

    println!(
        "{}@{}",
        crate::output::sanitize_line(login.as_deref().unwrap_or("?")),
        host_label(base_url)
    );
    println!("Saved API token to: {}", store_path.display());
    if let Some(kind) = outcome.kept_ui_login {
        println!(
            "Kept the existing {} login for UI-only commands; the token is used for REST API calls.",
            kind.as_str()
        );
    }
    if let Some(existing) = outcome.ui_username_mismatch {
        eprintln!(
            "warn: the token belongs to '{}' but the stored UI login is for '{}'.",
            crate::output::sanitize_line(login.as_deref().unwrap_or("?")),
            crate::output::sanitize_line(&existing)
        );
    }
    Ok(())
}

pub(crate) enum TokenValidation {
    /// Token authenticated as this user.
    User(String),
    /// Token authenticated, but `/api/v1/user` needs a scope it does not have.
    MissingUserScope,
}

/// Validate an API token with `GET /api/v1/user`. Server response bodies are
/// never included in errors because they may echo request data.
pub(crate) async fn validate_token(
    base_url: &str,
    token: &str,
    unix_socket: Option<&std::path::Path>,
) -> eyre::Result<TokenValidation> {
    let client = crate::api::ApiClient::new_with_socket(base_url, token, unix_socket)?;
    let url = client.api_v1_url("/user");
    let resp = client
        .http()
        .get(&url)
        .send()
        .await
        .wrap_err_with(|| format!("failed to reach '{base_url}' to validate the API token"))?;
    let status = resp.status();
    match status.as_u16() {
        200 => {
            let user: crate::api::AuthenticatedUser = resp
                .json()
                .await
                .wrap_err("failed to decode /api/v1/user response")?;
            Ok(TokenValidation::User(user.login))
        }
        // Forgejo/Gitea answer 403 for a valid token whose scopes exclude read:user.
        403 => Ok(TokenValidation::MissingUserScope),
        401 => Err(eyre!(
            "The API token was rejected by '{base_url}' (HTTP 401). Check that it is current and was created on this instance."
        )),
        _ => Err(eyre!(
            "Unexpected HTTP {status} from '{url}' while validating the API token."
        )),
    }
}

async fn run_session_import(
    args: LoginCommand,
    target: &crate::target::ResolvedTarget,
) -> eyre::Result<()> {
    let base_url = &target.base_url;
    let raw = read_secret_from_stdin("Session cookie (value or Cookie header)").await?;
    let parsed = parse_cookie_input(&raw)?;

    let candidates: Vec<Vec<(String, String)>> = match parsed {
        CookieInput::Pairs(pairs) => {
            if args.cookie_name.is_some() {
                eprintln!(
                    "warn: --cookie-name is ignored because the input already names its cookies."
                );
            }
            vec![pairs]
        }
        CookieInput::Bare(value) => match args.cookie_name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => vec![vec![(name.to_string(), value)]],
            Some(_) => return Err(eyre!("--cookie-name must not be empty")),
            None => DEFAULT_SESSION_COOKIE_NAMES
                .iter()
                .map(|name| vec![(name.to_string(), value.clone())])
                .collect(),
        },
    };

    for pairs in candidates {
        let probe = crate::session::UiSession::new_with_socket(
            base_url,
            None,
            target.unix_socket.as_deref(),
        )?;
        let jar = cookie_jar_for_pairs(
            &pairs,
            &probe.cookie_domain()?,
            &probe.cookie_path(),
            probe.is_https(),
        );
        let session = crate::session::UiSession::new_with_socket(
            base_url,
            Some(&jar),
            target.unix_socket.as_deref(),
        )?;
        if !session.test_session().await? {
            continue;
        }

        let login = session.fetch_login().await;
        // Store the jar as seen after validation (the server may have refreshed cookies).
        let jar = session.cookie_jar()?;
        crate::store::set_imported_session(base_url, login.as_deref(), jar).await?;
        let store_path = crate::store::ui_creds_store_paths()?.path;

        let names = pairs
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{}@{}",
            crate::output::sanitize_line(login.as_deref().unwrap_or("?")),
            host_label(base_url)
        );
        println!(
            "Imported browser session (cookie: {names}) to: {}",
            store_path.display()
        );
        println!(
            "Note: imported sessions cannot be renewed; when it expires, re-import it or use an API token (`--token-stdin`)."
        );
        return Ok(());
    }

    Err(eyre!(
        "The session cookie was not accepted by '{base_url}' (loading /user/settings redirected to the login page).\n\
         Copy the value of the session cookie from a logged-in browser tab (developer tools > Storage/Application > Cookies): \
         `session` on current Forgejo, `i_like_gitea` on Gitea and older Forgejo. You can also pass the whole Cookie header. \
         If the instance uses a custom [session] COOKIE_NAME, pass --cookie-name."
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum CookieInput {
    /// A bare cookie value; the cookie name is supplied separately.
    Bare(String),
    /// Explicit `name=value` pairs (single pair or a full Cookie header).
    Pairs(Vec<(String, String)>),
}

fn parse_cookie_input(raw: &str) -> eyre::Result<CookieInput> {
    let mut input = raw.trim();
    if input
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
    {
        input = input[7..].trim();
    }
    if input.is_empty() {
        return Err(eyre!("Session cookie input is empty."));
    }

    if !input.contains('=') {
        if input.contains(';') || input.chars().any(char::is_whitespace) {
            return Err(eyre!(
                "Session cookie value must not contain whitespace or ';'. Pass `name=value` pairs for multiple cookies."
            ));
        }
        return Ok(CookieInput::Bare(input.to_string()));
    }

    let mut pairs = Vec::new();
    for part in input.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part
            .split_once('=')
            .ok_or_else(|| eyre!("Invalid cookie fragment (expected name=value)."))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c == ',') {
            return Err(eyre!("Invalid cookie name in session cookie input."));
        }
        if value.is_empty() {
            continue;
        }
        pairs.push((name.to_string(), value.to_string()));
    }
    if pairs.is_empty() {
        return Err(eyre!("Session cookie input contains no cookie values."));
    }
    Ok(CookieInput::Pairs(pairs))
}

/// Imported cookies are scoped like the instance's own cookies (host-only, at
/// the instance base path), so a server `Set-Cookie` replaces them instead of
/// adding a second cookie with the same name.
fn cookie_jar_for_pairs(
    pairs: &[(String, String)],
    domain: &str,
    path: &str,
    secure: bool,
) -> crate::store::CookieJar {
    crate::store::CookieJar {
        saved_utc: None,
        cookies: pairs
            .iter()
            .map(|(name, value)| crate::store::CookieRecord {
                name: name.clone(),
                value: value.clone(),
                domain: domain.to_string(),
                host_only: true,
                path: path.to_string(),
                expires_utc: None,
                secure,
                http_only: true,
                same_site: Some("Lax".to_string()),
            })
            .collect(),
    }
}

/// Read a secret from stdin: the first line when piped, a no-echo prompt on a terminal.
async fn read_secret_from_stdin(label: &str) -> eyre::Result<String> {
    if std::io::stdin().is_terminal() {
        return prompt_password(label).await;
    }
    let lines = read_lines_from_stdin().await?;
    let value = lines
        .into_iter()
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if value.is_empty() {
        return Err(eyre!("No {} received on stdin.", label.to_lowercase()));
    }
    Ok(value)
}

async fn resolve_login_input(args: LoginCommand) -> eyre::Result<LoginInput> {
    let stdin_lines = if args.password_stdin || args.otp_stdin {
        read_lines_from_stdin().await?
    } else {
        Vec::new()
    };

    if let Some(userpass) = args.userpass.as_deref() {
        let idx = userpass
            .find(':')
            .ok_or_else(|| eyre::eyre!("Invalid --userpass format. Expected 'user:pass'."))?;
        if idx == 0 || idx >= userpass.len() - 1 {
            return Err(eyre::eyre!(
                "Invalid --userpass format. Expected 'user:pass'."
            ));
        }
        let username = userpass[..idx].to_string();
        let password = userpass[idx + 1..].to_string();
        let otp = resolve_otp(&args, &stdin_lines);
        return Ok(LoginInput {
            username,
            password,
            otp,
        });
    }

    let username = args
        .username
        .clone()
        .or_else(|| std::env::var("FJ_USER").ok())
        .unwrap_or_else(|| "".to_string());

    let username = if username.trim().is_empty() {
        prompt_line("Forgejo username").await?
    } else {
        username
    };

    let mut password = args
        .password
        .clone()
        .or_else(|| std::env::var("FJ_PASS").ok())
        .unwrap_or_else(|| "".to_string());

    if args.password_stdin && password.trim().is_empty() {
        password = stdin_lines.first().cloned().unwrap_or_default();
    }

    if password.trim().is_empty() {
        password = prompt_password("Forgejo password").await?;
    }

    if username.trim().is_empty() || password.trim().is_empty() {
        return Err(eyre::eyre!("Username/password must not be empty."));
    }

    let otp = resolve_otp(&args, &stdin_lines);

    Ok(LoginInput {
        username,
        password,
        otp,
    })
}

fn resolve_otp(args: &LoginCommand, stdin_lines: &[String]) -> Option<String> {
    let otp = args
        .otp
        .clone()
        .or_else(|| {
            if !args.otp_stdin {
                return None;
            }

            let index = if args.password_stdin { 1 } else { 0 };
            stdin_lines.get(index).cloned()
        })
        .or_else(|| std::env::var("FJ_OTP").ok());

    otp.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn prompt_line(label: &str) -> eyre::Result<String> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || -> eyre::Result<String> {
        use std::io::Write;
        print!("{label}: ");
        std::io::stdout().flush()?;
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        Ok(buf.trim().to_string())
    })
    .await?
}

/// The account needs a 2FA passcode, none was given, and prompting failed
/// (typically because there is no terminal, e.g. in CI).
fn two_factor_prompt_error(err: eyre::Report) -> eyre::Report {
    eyre!(
        "This account requires a two-factor passcode, but none was given and it could not be prompted for ({err}).\n\
         Pass it non-interactively with --otp-stdin (with --password-stdin: password on the first line, \
         passcode on the second), --otp <CODE>, or the FJ_OTP environment variable."
    )
}

async fn prompt_password(label: &str) -> eyre::Result<String> {
    let prompt = format!("{label}: ");
    tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt).map_err(Into::into))
        .await?
}

async fn read_lines_from_stdin() -> eyre::Result<Vec<String>> {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = String::new();
    stdin.read_to_string(&mut buf).await?;
    Ok(buf.lines().map(|line| line.trim().to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_factor_prompt_error_names_the_non_interactive_options() {
        let err = two_factor_prompt_error(eyre!("No such device or address (os error 6)"));
        let msg = err.to_string();
        assert!(msg.contains("two-factor passcode"));
        assert!(msg.contains("No such device or address"));
        for option in ["--otp-stdin", "--otp <CODE>", "FJ_OTP"] {
            assert!(msg.contains(option), "missing {option}: {msg}");
        }
    }

    #[test]
    fn otp_stdin_uses_second_line_when_password_stdin_is_enabled() {
        let mut args = test_login_command();
        args.password_stdin = true;
        args.otp_stdin = true;

        let lines = vec!["secret".to_string(), "123456".to_string()];

        assert_eq!(resolve_otp(&args, &lines).as_deref(), Some("123456"));
    }

    #[test]
    fn explicit_otp_argument_wins_over_stdin() {
        let mut args = test_login_command();
        args.otp = Some("654321".to_string());
        args.otp_stdin = true;

        let lines = vec!["123456".to_string()];

        assert_eq!(resolve_otp(&args, &lines).as_deref(), Some("654321"));
    }

    #[test]
    fn cookie_input_accepts_bare_value() {
        assert_eq!(
            parse_cookie_input("  abc123def \n").unwrap(),
            CookieInput::Bare("abc123def".to_string())
        );
    }

    #[test]
    fn cookie_input_accepts_name_value_and_cookie_headers() {
        assert_eq!(
            parse_cookie_input("i_like_gitea=xyz").unwrap(),
            CookieInput::Pairs(vec![("i_like_gitea".into(), "xyz".into())])
        );
        assert_eq!(
            parse_cookie_input("Cookie: i_like_forgejo=abc; _csrf=tok==; lang=en-US;").unwrap(),
            CookieInput::Pairs(vec![
                ("i_like_forgejo".into(), "abc".into()),
                ("_csrf".into(), "tok==".into()),
                ("lang".into(), "en-US".into()),
            ])
        );
    }

    #[test]
    fn cookie_input_rejects_malformed_values() {
        assert!(parse_cookie_input("").is_err());
        assert!(parse_cookie_input("Cookie:   ").is_err());
        assert!(parse_cookie_input("two words").is_err());
        assert!(parse_cookie_input("a=1; broken").is_err());
        assert!(parse_cookie_input("=value").is_err());
        assert!(parse_cookie_input("a=; b=").is_err());
    }

    #[test]
    fn imported_cookies_are_host_only_for_the_instance_domain() {
        let jar = cookie_jar_for_pairs(
            &[("i_like_forgejo".into(), "v".into())],
            "forge.example.com",
            "/",
            true,
        );
        assert_eq!(jar.cookies.len(), 1);
        let cookie = &jar.cookies[0];
        assert_eq!(cookie.domain, "forge.example.com");
        assert!(cookie.host_only);
        assert!(cookie.secure);
        assert_eq!(cookie.path, "/");
    }

    #[test]
    fn imported_cookies_use_the_instance_base_path() {
        let session =
            crate::session::UiSession::new("https://apps.example.com/gitea/", None).unwrap();
        assert_eq!(session.cookie_path(), "/gitea");
        let root = crate::session::UiSession::new("https://forge.example.com", None).unwrap();
        assert_eq!(root.cookie_path(), "/");

        let jar = cookie_jar_for_pairs(
            &[("i_like_gitea".into(), "v".into())],
            &session.cookie_domain().unwrap(),
            &session.cookie_path(),
            session.is_https(),
        );
        assert_eq!(jar.cookies[0].path, "/gitea");
        assert_eq!(jar.cookies[0].domain, "apps.example.com");
    }

    fn test_login_command() -> LoginCommand {
        LoginCommand {
            target: crate::cli::TargetArgs {
                host: None,
                repo: None,
                remote: None,
            },
            userpass: None,
            username: None,
            password: None,
            password_stdin: false,
            otp: None,
            otp_stdin: false,
            token: None,
            token_stdin: false,
            session_cookie_stdin: false,
            cookie_name: None,
        }
    }
}
