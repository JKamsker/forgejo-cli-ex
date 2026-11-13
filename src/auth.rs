use crate::cli::{
    AuthClearCookiesCommand, AuthCommand, AuthListCommand, AuthLogoutCommand,
    AuthNugetApiKeyCommand, AuthShowCommand, AuthStatusCommand, LoginCommand,
};
use eyre::{eyre, Context};
use time::OffsetDateTime;

pub async fn run(args: AuthCommand) -> eyre::Result<()> {
    match args {
        AuthCommand::Login(args) => crate::login::run(args).await,
        AuthCommand::Status(args) => run_status(args).await,
        AuthCommand::List(args) => run_list(args).await,
        AuthCommand::Show(args) => run_show(args).await,
        AuthCommand::Logout(args) => run_logout(args).await,
        AuthCommand::ClearCookies(args) => run_clear_cookies(args).await,
        AuthCommand::NugetApiKey(args) => run_nuget_api_key(args).await,
    }
}

pub async fn run_legacy_login(args: LoginCommand) -> eyre::Result<()> {
    eprintln!("warn: `fj-ex login` is deprecated; use `fj-ex auth login`.");
    crate::login::run(args).await
}

/// Outcome of checking the UI side of a login.
#[derive(Default)]
struct UiCheck {
    ok: bool,
    relogged: bool,
    error: Option<eyre::Report>,
}

/// Outcome of checking an API token.
struct TokenCheck {
    source: crate::credentials::TokenSource,
    ok: bool,
    user: Option<String>,
    missing_user_scope: bool,
    error: Option<eyre::Report>,
}

async fn run_status(args: AuthStatusCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url.clone();
    let host_key = crate::target::normalize_host_key(&base_url)?;
    let store_path = crate::store::ui_creds_store_paths()?.path;

    let entry = crate::store::get_store_entry(&base_url).await?.entry;
    let ui_kind = entry
        .as_ref()
        .and_then(crate::store::StoreEntry::ui_login_kind);
    let token = if ui_kind.is_some() {
        crate::credentials::resolve_api_token_lenient(&base_url).await
    } else {
        crate::credentials::resolve_api_token(&base_url).await?
    };

    // UI session (password login or imported browser session).
    let mut ui = UiCheck::default();
    if let Some(kind) = ui_kind {
        let jar = entry.as_ref().and_then(|e| e.cookie_jar.as_ref());
        if let Some(jar) = jar {
            let session = crate::session::UiSession::new_with_socket(
                &base_url,
                Some(jar),
                target.unix_socket.as_deref(),
            )?;
            ui.ok = session.test_session().await.unwrap_or(false);
        }

        if !ui.ok {
            match kind {
                crate::store::CredentialKind::Session => {
                    ui.error = Some(crate::session::session_expired_error(&base_url));
                }
                _ if args.no_relogin => {
                    ui.error = Some(eyre!(
                        "Not logged in to '{}' (cookie session invalid and --no-relogin specified).",
                        base_url
                    ));
                }
                _ => {
                    let relogin = async {
                        let session = crate::session::UiSession::from_store_with_socket(
                            &base_url,
                            true,
                            target.unix_socket.as_deref(),
                        )
                        .await?;
                        session.test_session().await
                    };
                    match relogin.await {
                        Ok(ok) => {
                            ui.ok = ok;
                            ui.relogged = true;
                        }
                        Err(err) => ui.error = Some(err),
                    }
                }
            }
        }
    }

    // API token (stored, environment, or fj keys.json).
    let token_check = match token {
        None => None,
        Some(token) => {
            let mut check = TokenCheck {
                source: token.source.clone(),
                ok: false,
                user: None,
                missing_user_scope: false,
                error: None,
            };
            match crate::login::validate_token(
                &base_url,
                &token.token,
                target.unix_socket.as_deref(),
            )
            .await
            {
                Ok(crate::login::TokenValidation::User(login)) => {
                    check.ok = true;
                    check.user = Some(login);
                }
                Ok(crate::login::TokenValidation::MissingUserScope) => {
                    check.ok = true;
                    check.missing_user_scope = true;
                }
                Err(err) => check.error = Some(err),
            }
            Some(check)
        }
    };

    let username = entry
        .as_ref()
        .and_then(|e| e.username.clone())
        .or_else(|| token_check.as_ref().and_then(|t| t.user.clone()))
        .unwrap_or_else(|| "?".to_string());
    let has_password = ui_kind == Some(crate::store::CredentialKind::Password);
    let token_ok = token_check.as_ref().is_some_and(|t| t.ok);
    let success = ui.ok || token_ok;

    if args.json {
        let payload = serde_json::json!({
            "baseUrl": base_url,
            "hostKey": host_key,
            "storePath": store_path.display().to_string(),
            "username": username,
            "credentialKind": entry
                .as_ref()
                .and_then(crate::store::StoreEntry::credential_kind)
                .map(|k| k.as_str()),
            "hasCreds": has_password,
            "hasCookieJar": entry.as_ref().is_some_and(|e| e.cookie_jar.is_some()),
            "hasSession": ui_kind.is_some(),
            "sessionOk": ui.ok,
            "relogged": ui.relogged,
            "hasStoredToken": entry.as_ref().is_some_and(crate::store::StoreEntry::has_api_token),
            "tokenSource": token_check.as_ref().map(|t| t.source.id()),
            "tokenOk": token_check.as_ref().map(|t| t.ok),
            "tokenUser": token_check.as_ref().and_then(|t| t.user.clone()),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }

    if ui_kind.is_none() && token_check.is_none() {
        return Err(eyre!(
            "No stored UI creds or API token for '{base_url}'. Run `fj-ex auth login --host {base_url}` \
             (password), `fj-ex auth login --host {base_url} --token-stdin` (API token), \
             `fj-ex auth login --host {base_url} --session-cookie-stdin` (SSO browser session), \
             or set FJ_EX_TOKEN/FORGEJO_TOKEN/GITEA_TOKEN."
        ));
    }

    if !success {
        if let Some(err) = ui.error {
            return Err(err);
        }
        if let Some(err) = token_check.and_then(|t| t.error) {
            return Err(err);
        }
        return Err(eyre!("Not logged in to '{}'.", base_url));
    }

    // Partial failures do not fail the command, but are reported.
    if let Some(err) = ui.error.as_ref() {
        eprintln!(
            "warn: UI session unusable: {}",
            err.to_string().lines().next().unwrap_or_default()
        );
    }
    if let Some(err) = token_check.as_ref().and_then(|t| t.error.as_ref()) {
        eprintln!(
            "warn: API token unusable: {}",
            err.to_string().lines().next().unwrap_or_default()
        );
    }

    if !args.json {
        println!("{}@{host_key}", crate::output::sanitize_line(&username));
        println!("OK");
        if let Some(kind) = ui_kind {
            let state = if ui.ok { "ok" } else { "unusable" };
            println!("UI session: {state} ({} login)", kind.as_str());
        }
        if let Some(check) = token_check.as_ref() {
            let state = if check.ok { "ok" } else { "rejected" };
            let note = if check.missing_user_scope {
                ", no read:user scope"
            } else {
                ""
            };
            println!("API token:  {state} ({}{note})", check.source);
        }
    }
    Ok(())
}

fn display_key(entry_key: &str, entry: &crate::store::StoreEntry) -> String {
    let base = entry.base_url.as_deref().unwrap_or(entry_key);
    match crate::target::normalize_base_url(base) {
        Ok(normalized) if crate::target::normalized_base_url_has_path(&normalized) => normalized,
        _ => crate::target::normalize_host_key(base).unwrap_or_else(|_| entry_key.to_string()),
    }
}

/// Groups store entries for `auth list`. Root URLs are shown by host key, but
/// when logins for different base URLs share one (`http://` and `https://` on
/// the same host), the non-https ones are shown by their full base URL so that
/// both stay visible. Only genuine duplicates (same base URL under legacy
/// keys) are merged, keeping the first.
fn entries_by_display_key(
    store: crate::store::CredsStore,
) -> std::collections::BTreeMap<String, crate::store::StoreEntry> {
    use std::collections::{BTreeMap, BTreeSet};

    let rows = store
        .into_iter()
        .map(|(key, entry)| {
            let display = display_key(&key, &entry);
            let base = entry.base_url.as_deref().unwrap_or(&key);
            let normalized =
                crate::target::normalize_base_url(base).unwrap_or_else(|_| base.to_string());
            (display, normalized, entry)
        })
        .collect::<Vec<_>>();

    let mut bases_by_display: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (display, normalized, _) in &rows {
        bases_by_display
            .entry(display.as_str())
            .or_default()
            .insert(normalized.as_str());
    }
    let shared: BTreeSet<String> = bases_by_display
        .into_iter()
        .filter(|(_, bases)| bases.len() > 1)
        .map(|(display, _)| display.to_string())
        .collect();

    let mut by_display = BTreeMap::new();
    for (display, normalized, entry) in rows {
        let display = if shared.contains(&display) && !normalized.starts_with("https://") {
            normalized
        } else {
            display
        };
        by_display.entry(display).or_insert(entry);
    }
    by_display
}

async fn run_list(args: AuthListCommand) -> eyre::Result<()> {
    let store_path = crate::store::ui_creds_store_paths()?.path;
    let store = crate::store::read_creds_store()
        .await
        .wrap_err("failed to read creds store")?;

    let by_host = entries_by_display_key(store);

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
                    "credentialKind": entry.credential_kind().map(|k| k.as_str()),
                    "credentials": entry.credential_summary(),
                    "hasPassword": entry.ui_login_kind() == Some(crate::store::CredentialKind::Password),
                    "hasToken": entry.has_api_token(),
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
        let summary = entry.credential_summary();
        let username = entry.username.unwrap_or_else(|| "?".to_string());
        println!(
            "{}@{host_key} ({summary})",
            crate::output::sanitize_line(&username)
        );
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
    let kind = entry.credential_kind().map(|k| k.as_str());
    let summary = entry.credential_summary();
    let has_token = entry.has_api_token();

    if args.unsafe_show_password {
        eprintln!("warn: --unsafe-show-password prints the plaintext password to stdout.");
    }

    if args.json {
        let mut payload = serde_json::json!({
            "baseUrl": base_url,
            "hostKey": host_key,
            "storePath": store_path.display().to_string(),
            "username": entry.username,
            "updatedUtc": entry.updated_utc,
            "credentialKind": kind,
            "credentials": summary,
            "hasPassword": entry.password.as_ref().map(|p| !p.is_empty()).unwrap_or(false),
            "hasToken": has_token,
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
    let username = crate::output::sanitize_line(&username);
    println!("Host:      {base_url}");
    println!("HostKey:   {host_key}");
    println!("Username:  {username}");
    println!("Kind:      {summary}");
    println!(
        "Token:     {}",
        if has_token {
            "stored (value hidden)"
        } else {
            "none"
        }
    );
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
        println!(
            "signed out of {}@{host_key}",
            crate::output::sanitize_line(&username)
        );
    } else {
        println!("already signed out of {host_key}");
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

    let was_session = crate::store::get_store_entry(&base_url)
        .await?
        .entry
        .as_ref()
        .and_then(crate::store::StoreEntry::ui_login_kind)
        == Some(crate::store::CredentialKind::Session);
    crate::store::clear_cookie_jar(&base_url).await?;
    if was_session {
        println!(
            "Removed the imported browser session for {host_key} (a stored API token is kept)"
        );
    } else {
        println!("Cleared cookies for {host_key}");
    }
    Ok(())
}

pub async fn run_nuget_api_key(args: AuthNugetApiKeyCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;
    let base_url = target.base_url;

    // Creating access tokens through the API requires HTTP basic auth with the
    // account password, so this needs a password login.
    let creds = crate::store::get_ui_creds(&base_url)
        .await?
        .ok_or_else(|| {
            eyre!(
                "No stored password login for '{}'. Run `fj-ex auth login --host {}` first \
                 (the API only allows creating tokens with username + password; SSO accounts \
                 can create a package token in the web UI under Settings > Applications).",
                base_url,
                base_url
            )
        })?;

    let username = creds.username.trim().to_string();
    if username.is_empty() {
        return Err(eyre!(
            "Stored UI creds for '{}' are missing a username. Re-run `fj-ex auth login`.",
            base_url
        ));
    }

    let owner = args.owner.unwrap_or_else(|| username.clone());
    let owner = owner.trim().to_string();
    if owner.is_empty() {
        return Err(eyre!("--owner cannot be empty"));
    }

    let token_name = args
        .token_name
        .unwrap_or_else(default_nuget_token_name)
        .trim()
        .to_string();
    if token_name.is_empty() {
        return Err(eyre!("--token-name cannot be empty"));
    }

    let scope = if args.read_only {
        "read:package"
    } else {
        "write:package"
    };

    let client = crate::api::ApiClient::from_auth(
        &base_url,
        &crate::credentials::ApiAuth::Basic(creds.clone()),
        target.unix_socket.as_deref(),
    )?;
    let url = client.api_v1_url(&format!("/users/{}/tokens", urlencoding::encode(&username)));
    let body = serde_json::json!({
        "name": token_name,
        "scopes": [scope],
    });
    let created: crate::api::CreatedAccessToken = client
        .post_json_with_basic_auth(&url, &body, &username, &creds.password)
        .await?;

    let registry_url = format!(
        "{}/api/packages/{}/nuget/index.json",
        base_url.trim_end_matches('/'),
        urlencoding::encode(&owner)
    );

    if args.json {
        let payload = serde_json::json!({
            "baseUrl": &base_url,
            "owner": &owner,
            "username": &username,
            "registryUrl": &registry_url,
            "tokenName": &created.name,
            "scope": scope,
            "apiKey": &created.token,
            "tokenLastEight": &created.token_last_eight,
            "env": {
                "FORGEJO_NUGET_USERNAME": &username,
                "FORGEJO_NUGET_SOURCE": &registry_url,
                "FORGEJO_NUGET_API_KEY": &created.token,
            }
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("FORGEJO_NUGET_USERNAME={username}");
    println!("FORGEJO_NUGET_SOURCE={registry_url}");
    println!("FORGEJO_NUGET_API_KEY={}", created.token);
    Ok(())
}

fn default_nuget_token_name() -> String {
    format!(
        "fj-ex-nuget-{}",
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CredentialKind, CredsStore, StoreEntry};

    fn token_entry(base_url: &str, token: &str) -> StoreEntry {
        StoreEntry {
            base_url: Some(base_url.to_string()),
            kind: Some(CredentialKind::Token),
            api_token: Some(token.to_string()),
            ..StoreEntry::default()
        }
    }

    #[test]
    fn list_shows_http_and_https_logins_for_one_host() {
        let mut store = CredsStore::default();
        store.insert(
            "forge.example.com".to_string(),
            token_entry("https://forge.example.com", "a"),
        );
        store.insert(
            "http://forge.example.com".to_string(),
            token_entry("http://forge.example.com", "b"),
        );

        let listed = entries_by_display_key(store);
        let keys = listed.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys, vec!["forge.example.com", "http://forge.example.com"]);
        assert_eq!(listed["forge.example.com"].api_token.as_deref(), Some("a"));
        assert_eq!(
            listed["http://forge.example.com"].api_token.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn list_merges_legacy_duplicates_and_keeps_plain_host_keys() {
        let mut store = CredsStore::default();
        store.insert(
            "forge.example.com".to_string(),
            token_entry("https://forge.example.com", "a"),
        );
        store.insert(
            "https://forge.example.com".to_string(),
            token_entry("https://forge.example.com", "dup"),
        );
        store.insert(
            "localhost:3000".to_string(),
            token_entry("http://localhost:3000", "c"),
        );

        let listed = entries_by_display_key(store);
        let keys = listed.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys, vec!["forge.example.com", "localhost:3000"]);
        assert_eq!(listed["forge.example.com"].api_token.as_deref(), Some("a"));
    }
}
