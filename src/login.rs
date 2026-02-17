use crate::cli::LoginCommand;

pub async fn run(args: LoginCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url;

    let (username, password) = resolve_creds(args).await?;

    // Validate login by actually logging in via UI.
    let session = crate::session::UiSession::new(&base_url, None)?;
    session.login_with_creds(&username, &password).await?;

    // Persist plaintext creds (required by design).
    crate::store::set_ui_creds(&base_url, &username, &password).await?;

    // Persist cookies.
    let _ = session.persist_cookie_jar().await;

    let store_path = crate::store::ui_creds_store_paths()?.path;
    let host_label = url::Url::parse(&base_url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| base_url.clone());

    println!("{username}@{host_label}");
    println!("Saved UI creds to: {}", store_path.display());

    Ok(())
}

async fn resolve_creds(args: LoginCommand) -> eyre::Result<(String, String)> {
    if let Some(userpass) = args.userpass {
        let idx = userpass.find(':').ok_or_else(|| {
            eyre::eyre!("Invalid --userpass format. Expected 'user:pass'.")
        })?;
        if idx == 0 || idx >= userpass.len() - 1 {
            return Err(eyre::eyre!("Invalid --userpass format. Expected 'user:pass'."));
        }
        let username = userpass[..idx].to_string();
        let password = userpass[idx + 1..].to_string();
        return Ok((username, password));
    }

    let username = args
        .username
        .or_else(|| std::env::var("FJ_USER").ok())
        .unwrap_or_else(|| "".to_string());

    let username = if username.trim().is_empty() {
        prompt_line("Forgejo username").await?
    } else {
        username
    };

    let mut password = args
        .password
        .or_else(|| std::env::var("FJ_PASS").ok())
        .unwrap_or_else(|| "".to_string());

    if args.password_stdin && password.trim().is_empty() {
        password = read_line_from_stdin().await?.unwrap_or_default();
    }

    if password.trim().is_empty() {
        password = prompt_password("Forgejo password").await?;
    }

    if username.trim().is_empty() || password.trim().is_empty() {
        return Err(eyre::eyre!("Username/password must not be empty."));
    }

    Ok((username, password))
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

async fn read_line_from_stdin() -> eyre::Result<Option<String>> {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = String::new();
    stdin.read_to_string(&mut buf).await?;
    let s = buf.lines().next().unwrap_or("").trim().to_string();
    if s.is_empty() {
        Ok(None)
    } else {
        Ok(Some(s))
    }
}
