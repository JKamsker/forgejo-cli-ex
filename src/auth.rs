use crate::cli::{
    AuthClearCookiesCommand, AuthCommand, AuthListCommand, AuthLogoutCommand, AuthShowCommand,
    AuthStatusCommand, LoginCommand,
};
use eyre::{eyre, Context};

pub async fn run(args: AuthCommand) -> eyre::Result<()> {
    match args {
        AuthCommand::Login(args) => crate::login::run(args).await,
        AuthCommand::Status(args) => run_status(args).await,
        AuthCommand::List(args) => run_list(args).await,
        AuthCommand::Show(args) => run_show(args).await,
        AuthCommand::Logout(args) => run_logout(args).await,
        AuthCommand::ClearCookies(args) => run_clear_cookies(args).await,
    }
}

pub async fn run_legacy_login(args: LoginCommand) -> eyre::Result<()> {
    eprintln!("warn: `fj-ex login` is deprecated; use `fj-ex auth login`.");
    crate::login::run(args).await
}

async fn run_status(args: AuthStatusCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url;
    let host_key = crate::target::normalize_host_key(&base_url)?;
    let store_path = crate::store::ui_creds_store_paths()?.path;

    let info = crate::store::get_store_entry(&base_url).await?;
    let Some(entry) = info.entry else {
        if args.json {
            let payload = serde_json::json!({
                "baseUrl": base_url,
                "hostKey": host_key,
                "storePath": store_path.display().to_string(),
                "hasCreds": false,
                "hasCookieJar": false,
                "sessionOk": false,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        }
        return Err(eyre!(
            "No stored UI creds for '{}'. Run `fj-ex auth login` first.",
            base_url
        ));
    };

    let username = entry.username.clone().unwrap_or_else(|| "?".to_string());
    let has_cookie_jar = entry.cookie_jar.is_some();

    let mut session_ok = false;
    let mut relogged = false;

    if let Some(jar) = entry.cookie_jar.as_ref() {
        let session = crate::session::UiSession::new(&base_url, Some(jar))?;
        session_ok = session.test_session().await.unwrap_or(false);
    }

    if !session_ok {
        if args.no_relogin {
            if args.json {
                let payload = serde_json::json!({
                    "baseUrl": base_url,
                    "hostKey": host_key,
                    "storePath": store_path.display().to_string(),
                    "username": username,
                    "hasCreds": true,
                    "hasCookieJar": has_cookie_jar,
                    "sessionOk": false,
                    "relogged": false,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            }
            return Err(eyre!(
                "Not logged in to '{}' (cookie session invalid and --no-relogin specified).",
                base_url
            ));
        }

        let session = crate::session::UiSession::from_store(&base_url, true).await?;
        session_ok = session.test_session().await?;
        relogged = true;
    }

    if args.json {
        let payload = serde_json::json!({
            "baseUrl": base_url,
            "hostKey": host_key,
            "storePath": store_path.display().to_string(),
            "username": username,
            "hasCreds": true,
            "hasCookieJar": has_cookie_jar,
            "sessionOk": session_ok,
            "relogged": relogged,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("{username}@{host_key}");
    println!("OK");
    Ok(())
}

async fn run_list(args: AuthListCommand) -> eyre::Result<()> {
    let store_path = crate::store::ui_creds_store_paths()?.path;
    let store = crate::store::read_creds_store()
        .await
        .wrap_err("failed to read creds store")?;

    let mut by_host: std::collections::BTreeMap<String, crate::store::StoreEntry> =
        std::collections::BTreeMap::new();
    for (key, entry) in store {
        let base = entry.base_url.as_deref().unwrap_or(&key);
        let host_key = crate::target::normalize_host_key(base).unwrap_or(key);
        by_host.entry(host_key).or_insert(entry);
    }

    if args.json {
        let logins = by_host
            .iter()
            .map(|(host_key, entry)| {
                let base_url = entry
                    .base_url
                    .clone()
                    .unwrap_or_else(|| format!("https://{host_key}"));
                serde_json::json!({
                    "hostKey": host_key,
                    "baseUrl": base_url,
                    "username": entry.username.clone(),
                    "updatedUtc": entry.updated_utc.clone(),
                    "hasCookieJar": entry.cookie_jar.is_some(),
                })
            })
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "storePath": store_path.display().to_string(),
            "logins": logins,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if by_host.is_empty() {
        println!("No logins.");
        return Ok(());
    }

    for (host_key, entry) in by_host {
        let username = entry.username.unwrap_or_else(|| "?".to_string());
        println!("{username}@{host_key}");
    }
    Ok(())
}

async fn run_show(args: AuthShowCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url;
    let host_key = crate::target::normalize_host_key(&base_url)?;
    let store_path = crate::store::ui_creds_store_paths()?.path;

    let info = crate::store::get_store_entry(&base_url).await?;
    let entry = info.entry.ok_or_else(|| {
        eyre!(
            "No stored UI creds for '{}'. Run `fj-ex auth login` first.",
            base_url
        )
    })?;

    let cookie_summary = entry.cookie_jar.as_ref().map(|jar| {
        serde_json::json!({
            "savedUtc": jar.saved_utc,
            "cookieCount": jar.cookies.len(),
        })
    });

    if args.json {
        let mut payload = serde_json::json!({
            "baseUrl": base_url,
            "hostKey": host_key,
            "storePath": store_path.display().to_string(),
            "username": entry.username,
            "updatedUtc": entry.updated_utc,
            "hasPassword": entry.password.as_ref().map(|p| !p.is_empty()).unwrap_or(false),
            "cookieJar": cookie_summary,
        });

        if args.unsafe_show_password {
            if let Some(p) = entry.password {
                payload["password"] = serde_json::Value::String(p);
            }
        }

        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let username = entry.username.unwrap_or_else(|| "?".to_string());
    println!("Host:      {base_url}");
    println!("HostKey:   {host_key}");
    println!("Username:  {username}");
    println!(
        "Updated:   {}",
        entry.updated_utc.unwrap_or_else(|| "?".to_string())
    );
    println!(
        "Cookies:   {}",
        entry
            .cookie_jar
            .as_ref()
            .map(|j| j.cookies.len())
            .unwrap_or(0)
    );
    println!("Store:     {}", store_path.display());

    if args.unsafe_show_password {
        let password = entry.password.unwrap_or_else(|| "".to_string());
        println!("Password:  {password}");
    }

    Ok(())
}

async fn run_logout(args: AuthLogoutCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url;
    let host_key = crate::target::normalize_host_key(&base_url)?;

    let removed = crate::store::delete_store_entry(&base_url).await?;
    if let Some(entry) = removed {
        let username = entry.username.unwrap_or_else(|| "?".to_string());
        println!("signed out of {username}@{host_key}");
    } else {
        println!("already not signed in to {host_key}");
    }
    Ok(())
}

async fn run_clear_cookies(args: AuthClearCookiesCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url;
    let host_key = crate::target::normalize_host_key(&base_url)?;

    crate::store::clear_cookie_jar(&base_url).await?;
    println!("Cleared cookies for {host_key}");
    Ok(())
}
