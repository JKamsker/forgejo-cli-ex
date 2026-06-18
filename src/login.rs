use crate::cli::LoginCommand;

struct LoginInput {
    username: String,
    password: String,
    otp: Option<String>,
}

pub async fn run(args: LoginCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
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
            let passcode = prompt_password("Forgejo 2FA passcode").await?;
            session.submit_two_factor_code(&passcode).await?;
        }
    }

    // Persist plaintext creds (required by design).
    crate::store::set_ui_creds(&base_url, &input.username, &input.password).await?;

    // Persist cookies.
    session.persist_cookie_jar_required().await?;

    let store_path = crate::store::ui_creds_store_paths()?.path;
    let host_label = url::Url::parse(&base_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| base_url.clone());

    println!("{}@{host_label}", input.username);
    println!("Saved UI creds to: {}", store_path.display());

    Ok(())
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
        }
    }
}
