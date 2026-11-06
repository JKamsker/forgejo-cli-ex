use std::{path::Path, sync::Arc, time::Duration};

use super::{
    creds::{
        clear_cookie_jar_with_paths, get_store_entry_with_paths,
        save_cookie_jar_required_with_paths, save_cookie_jar_with_paths, set_api_token_with_paths,
        set_imported_session_with_paths, set_ui_creds_with_paths, CookieJar, CookieRecord,
        CredentialKind, CredsStore, StoreEntry,
    },
    file::{read_creds_store_with_paths, update_creds_store},
    lock::{acquire_store_lock, StoreLockMode},
    StorePaths,
};

#[test]
fn save_cookie_jar_without_existing_creds_does_not_create_cookie_only_entry() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    save_cookie_jar_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("session-a"),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(store.is_empty());
}

#[test]
fn save_cookie_jar_preserves_existing_creds() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();
    save_cookie_jar_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("session-a"),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.password.as_deref(), Some("secret"));
    assert_eq!(entry.user_pass.as_deref(), Some("alice:secret"));
    assert_eq!(
        entry
            .cookie_jar
            .as_ref()
            .map(|jar| jar.cookies.len())
            .unwrap_or_default(),
        1
    );
}

#[test]
fn required_cookie_save_preserves_existing_creds() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();
    save_cookie_jar_required_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("session-a"),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.password.as_deref(), Some("secret"));
    assert!(entry.cookie_jar.is_some());
}

#[test]
fn cookie_only_entries_are_removed_before_writing_store() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    update_creds_store(&paths, StoreLockMode::Required, |store| {
        store.insert(
            "forge.example.com".to_string(),
            StoreEntry {
                base_url: Some("https://forge.example.com".to_string()),
                cookie_jar: Some(test_cookie_jar("session-a")),
                ..StoreEntry::default()
            },
        );
        Ok(((), true))
    })
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(!store.contains_key("forge.example.com"));
    assert!(store.is_empty());
}

#[test]
fn save_cookie_jar_removes_existing_cookie_only_entry() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::create_dir_all(&paths.dir).unwrap();

    let mut broken_store = CredsStore::default();
    broken_store.insert(
        "forge.example.com".to_string(),
        StoreEntry {
            base_url: Some("https://forge.example.com".to_string()),
            cookie_jar: Some(test_cookie_jar("old-session")),
            ..StoreEntry::default()
        },
    );
    std::fs::write(
        &paths.path,
        serde_json::to_vec_pretty(&broken_store).unwrap(),
    )
    .unwrap();

    save_cookie_jar_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("new-session"),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(store.is_empty());
}

#[test]
fn read_creds_store_removes_cookie_only_entry() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::create_dir_all(&paths.dir).unwrap();

    let mut broken_store = CredsStore::default();
    broken_store.insert(
        "forge.example.com".to_string(),
        StoreEntry {
            base_url: Some("https://forge.example.com".to_string()),
            cookie_jar: Some(test_cookie_jar("old-session")),
            ..StoreEntry::default()
        },
    );
    std::fs::write(
        &paths.path,
        serde_json::to_vec_pretty(&broken_store).unwrap(),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(store.is_empty());

    let raw = std::fs::read_to_string(&paths.path).unwrap();
    let persisted = serde_json::from_str::<CredsStore>(&raw).unwrap();
    assert!(persisted.is_empty());
}

#[test]
fn concurrent_optional_cookie_saves_preserve_creds_and_valid_json() {
    let temp = tempfile::tempdir().unwrap();
    let paths = Arc::new(test_store_paths(temp.path()));
    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();

    let handles = (0..24)
        .map(|index| {
            let paths = Arc::clone(&paths);
            std::thread::spawn(move || {
                save_cookie_jar_with_paths(
                    &paths,
                    "https://forge.example.com",
                    test_cookie_jar(&format!("session-{index}")),
                )
                .unwrap();
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().unwrap();
    }

    let raw = std::fs::read_to_string(&paths.path).unwrap();
    serde_json::from_str::<CredsStore>(&raw).unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.password.as_deref(), Some("secret"));
    if let Some(cookie_jar) = &entry.cookie_jar {
        assert_eq!(cookie_jar.cookies.len(), 1);
        assert!(cookie_jar.cookies[0].value.starts_with("session-"));
    }
}

#[test]
fn parallel_required_and_optional_updates_keep_store_valid_json() {
    let temp = tempfile::tempdir().unwrap();
    let paths = Arc::new(test_store_paths(temp.path()));
    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();

    let mut handles = Vec::new();

    for index in 0..12 {
        let paths = Arc::clone(&paths);
        handles.push(std::thread::spawn(move || {
            set_ui_creds_with_paths(
                &paths,
                "https://forge.example.com",
                &format!("alice-{index}"),
                "secret",
            )
            .unwrap();
        }));
    }

    for index in 0..24 {
        let paths = Arc::clone(&paths);
        handles.push(std::thread::spawn(move || {
            save_cookie_jar_with_paths(
                &paths,
                "https://forge.example.com",
                test_cookie_jar(&format!("session-{index}")),
            )
            .unwrap();
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let raw = std::fs::read_to_string(&paths.path).unwrap();
    serde_json::from_str::<CredsStore>(&raw).unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert!(entry
        .username
        .as_deref()
        .unwrap_or_default()
        .starts_with("alice"));
    assert_eq!(entry.password.as_deref(), Some("secret"));
}

#[test]
fn path_base_url_uses_full_url_store_key() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_ui_creds_with_paths(&paths, "https://apps.example.com/gitea", "alice", "secret").unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("https://apps.example.com/gitea").unwrap();
    assert_eq!(
        entry.base_url.as_deref(),
        Some("https://apps.example.com/gitea")
    );
    assert_eq!(entry.username.as_deref(), Some("alice"));
}

#[test]
fn path_base_url_can_find_legacy_entry_by_stored_base_url() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::create_dir_all(&paths.dir).unwrap();

    let mut legacy_store = CredsStore::default();
    legacy_store.insert(
        "https://apps.example.com".to_string(),
        StoreEntry {
            base_url: Some("https://apps.example.com/gitea/".to_string()),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            user_pass: Some("alice:secret".to_string()),
            ..StoreEntry::default()
        },
    );
    std::fs::write(
        &paths.path,
        serde_json::to_vec_pretty(&legacy_store).unwrap(),
    )
    .unwrap();

    let info = get_store_entry_with_paths(&paths, "https://apps.example.com/gitea").unwrap();
    let entry = info.entry.unwrap();
    assert_eq!(
        entry.base_url.as_deref(),
        Some("https://apps.example.com/gitea/")
    );
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.password.as_deref(), Some("secret"));
}

#[test]
fn optional_cookie_save_does_not_wait_for_busy_store_lock() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();

    let lock = acquire_store_lock(&paths, StoreLockMode::Required)
        .unwrap()
        .unwrap();

    let started = std::time::Instant::now();
    save_cookie_jar_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("session-a"),
    )
    .unwrap();

    assert!(
        started.elapsed() < Duration::from_millis(100),
        "optional cookie save waited for a busy creds lock"
    );

    drop(lock);

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert!(entry.cookie_jar.is_none());
}

#[test]
fn path_base_url_login_migrates_legacy_entry_and_preserves_cookie() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::create_dir_all(&paths.dir).unwrap();

    let mut legacy_store = CredsStore::default();
    legacy_store.insert(
        "https://apps.example.com".to_string(),
        StoreEntry {
            base_url: Some("https://apps.example.com/gitea/".to_string()),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            user_pass: Some("alice:secret".to_string()),
            cookie_jar: Some(test_cookie_jar("session-a")),
            ..StoreEntry::default()
        },
    );
    std::fs::write(
        &paths.path,
        serde_json::to_vec_pretty(&legacy_store).unwrap(),
    )
    .unwrap();

    set_ui_creds_with_paths(
        &paths,
        "https://apps.example.com/gitea",
        "alice",
        "new-secret",
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(!store.contains_key("https://apps.example.com"));
    let entry = store.get("https://apps.example.com/gitea").unwrap();
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.password.as_deref(), Some("new-secret"));
    assert!(entry.cookie_jar.is_some());
}

// ---------------------------------------------------------------------------
// Credential kinds (password / imported session / token)
// ---------------------------------------------------------------------------

#[test]
fn legacy_store_without_kind_fields_is_read_as_password_login() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::create_dir_all(&paths.dir).unwrap();
    // A file as written by fj-ex <= 0.1.12: no `kind`, no `apiToken`.
    std::fs::write(
        &paths.path,
        r#"{
  "forge.example.com": {
    "baseUrl": "https://forge.example.com",
    "username": "alice",
    "password": "secret",
    "userPass": "alice:secret",
    "updatedUtc": "2026-01-01T00:00:00Z",
    "cookieJar": { "savedUtc": null, "cookies": [] },
    "someFutureField": 42
  }
}"#,
    )
    .unwrap();

    let entry = get_store_entry_with_paths(&paths, "https://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(entry.kind, None);
    assert_eq!(entry.credential_kind(), Some(CredentialKind::Password));
    assert_eq!(entry.ui_login_kind(), Some(CredentialKind::Password));
    assert!(!entry.has_api_token());
    assert_eq!(
        entry.extra.get("someFutureField"),
        Some(&serde_json::json!(42))
    );

    // Reading must not rewrite or drop the legacy entry.
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.path).unwrap()).unwrap();
    assert!(raw["forge.example.com"].get("kind").is_none());

    // The first cookie save pins the kind and keeps unknown fields.
    save_cookie_jar_with_paths(&paths, "https://forge.example.com", test_cookie_jar("s")).unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.path).unwrap()).unwrap();
    assert_eq!(raw["forge.example.com"]["kind"], "password");
    assert_eq!(raw["forge.example.com"]["someFutureField"], 42);
    assert!(raw["forge.example.com"].get("apiToken").is_none());
}

#[test]
fn imported_session_survives_reads_and_cookie_refreshes() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_imported_session_with_paths(
        &paths,
        "https://forge.example.com",
        Some("sso-user"),
        test_cookie_jar("browser-session"),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert_eq!(entry.kind, Some(CredentialKind::Session));
    assert_eq!(entry.ui_login_kind(), Some(CredentialKind::Session));
    assert_eq!(entry.username.as_deref(), Some("sso-user"));
    assert_eq!(entry.password, None);

    // Optional and required cookie saves keep the session entry.
    save_cookie_jar_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("refreshed"),
    )
    .unwrap();
    save_cookie_jar_required_with_paths(
        &paths,
        "https://forge.example.com",
        test_cookie_jar("refreshed-2"),
    )
    .unwrap();
    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert_eq!(entry.kind, Some(CredentialKind::Session));
    assert_eq!(
        entry.cookie_jar.as_ref().unwrap().cookies[0].value,
        "refreshed-2"
    );
}

#[test]
fn session_kind_without_cookies_is_repaired_away() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::create_dir_all(&paths.dir).unwrap();

    let mut broken = CredsStore::default();
    broken.insert(
        "forge.example.com".to_string(),
        StoreEntry {
            base_url: Some("https://forge.example.com".to_string()),
            kind: Some(CredentialKind::Session),
            cookie_jar: Some(CookieJar::default()),
            ..StoreEntry::default()
        },
    );
    broken.insert(
        "tok.example.com".to_string(),
        StoreEntry {
            base_url: Some("https://tok.example.com".to_string()),
            kind: Some(CredentialKind::Token),
            ..StoreEntry::default()
        },
    );
    std::fs::write(&paths.path, serde_json::to_vec_pretty(&broken).unwrap()).unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(store.is_empty());
}

#[test]
fn token_only_login_survives_and_never_gets_cookies() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    let outcome =
        set_api_token_with_paths(&paths, "https://forge.example.com", " tok-1 \n", Some("ci"))
            .unwrap();
    assert_eq!(outcome.kept_ui_login, None);

    save_cookie_jar_with_paths(&paths, "https://forge.example.com", test_cookie_jar("x")).unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    let entry = store.get("forge.example.com").unwrap();
    assert_eq!(entry.kind, Some(CredentialKind::Token));
    assert_eq!(entry.api_token.as_deref(), Some("tok-1"));
    assert_eq!(entry.username.as_deref(), Some("ci"));
    assert_eq!(entry.ui_login_kind(), None);
    assert!(entry.cookie_jar.is_none());
    assert_eq!(entry.credential_summary(), "token");
}

#[test]
fn token_and_password_logins_are_kept_side_by_side() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();
    let outcome =
        set_api_token_with_paths(&paths, "https://forge.example.com", "tok", Some("bob")).unwrap();
    assert_eq!(outcome.kept_ui_login, Some(CredentialKind::Password));
    assert_eq!(outcome.ui_username_mismatch.as_deref(), Some("alice"));

    let entry = get_store_entry_with_paths(&paths, "https://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(entry.kind, Some(CredentialKind::Password));
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.password.as_deref(), Some("secret"));
    assert_eq!(entry.api_token.as_deref(), Some("tok"));
    assert_eq!(entry.credential_summary(), "password + token");

    // A later password login keeps the token.
    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "rotated").unwrap();
    let entry = get_store_entry_with_paths(&paths, "https://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(entry.password.as_deref(), Some("rotated"));
    assert_eq!(entry.api_token.as_deref(), Some("tok"));
}

#[test]
fn session_import_replaces_password_but_keeps_token() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();
    set_api_token_with_paths(&paths, "https://forge.example.com", "tok", None).unwrap();
    set_imported_session_with_paths(
        &paths,
        "https://forge.example.com",
        None,
        test_cookie_jar("sess"),
    )
    .unwrap();

    let entry = get_store_entry_with_paths(&paths, "https://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(entry.kind, Some(CredentialKind::Session));
    assert_eq!(entry.password, None);
    assert_eq!(entry.user_pass, None);
    assert_eq!(entry.username.as_deref(), Some("alice"));
    assert_eq!(entry.api_token.as_deref(), Some("tok"));
}

#[test]
fn clearing_cookies_of_an_imported_session_removes_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_imported_session_with_paths(&paths, "https://a.example.com", None, test_cookie_jar("a"))
        .unwrap();
    set_imported_session_with_paths(&paths, "https://b.example.com", None, test_cookie_jar("b"))
        .unwrap();
    set_api_token_with_paths(&paths, "https://b.example.com", "tok", None).unwrap();

    clear_cookie_jar_with_paths(&paths, "https://a.example.com").unwrap();
    clear_cookie_jar_with_paths(&paths, "https://b.example.com").unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(!store.contains_key("a.example.com"));
    let b = store.get("b.example.com").unwrap();
    assert_eq!(b.kind, Some(CredentialKind::Token));
    assert!(b.cookie_jar.is_none());
}

#[test]
fn clearing_cookies_keeps_password_logins() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "secret").unwrap();
    save_cookie_jar_with_paths(&paths, "https://forge.example.com", test_cookie_jar("s")).unwrap();

    clear_cookie_jar_with_paths(&paths, "https://forge.example.com").unwrap();

    let entry = get_store_entry_with_paths(&paths, "https://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    assert!(entry.cookie_jar.is_none());
    assert_eq!(entry.password.as_deref(), Some("secret"));
}

#[test]
fn tokens_and_sessions_are_keyed_by_base_url_including_path() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_api_token_with_paths(&paths, "https://apps.example.com/gitea/", "gitea-tok", None).unwrap();
    set_api_token_with_paths(&paths, "https://apps.example.com", "root-tok", None).unwrap();
    set_imported_session_with_paths(
        &paths,
        "https://apps.example.com/forgejo",
        None,
        test_cookie_jar("fj"),
    )
    .unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert_eq!(
        store["https://apps.example.com/gitea"].api_token.as_deref(),
        Some("gitea-tok")
    );
    assert_eq!(
        store["apps.example.com"].api_token.as_deref(),
        Some("root-tok")
    );
    assert_eq!(
        store["https://apps.example.com/forgejo"].kind,
        Some(CredentialKind::Session)
    );

    let gitea = get_store_entry_with_paths(&paths, "https://apps.example.com/gitea")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(gitea.api_token.as_deref(), Some("gitea-tok"));
    // A path deployment never falls back to the root login's token.
    let other = get_store_entry_with_paths(&paths, "https://apps.example.com/other")
        .unwrap()
        .entry;
    assert!(other.is_none());
}

#[test]
fn logins_for_http_and_https_on_one_host_do_not_overwrite_each_other() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());

    set_api_token_with_paths(&paths, "https://forge.example.com", "https-tok", None).unwrap();
    set_api_token_with_paths(&paths, "http://forge.example.com", "http-tok", None).unwrap();

    let https = get_store_entry_with_paths(&paths, "https://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    let http = get_store_entry_with_paths(&paths, "http://forge.example.com")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(https.api_token.as_deref(), Some("https-tok"));
    assert_eq!(http.api_token.as_deref(), Some("http-tok"));
}

#[test]
fn unix_socket_logins_are_keyed_by_socket_url() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    let socket_url = "http+unix:///run/forgejo/http.sock";

    set_api_token_with_paths(&paths, socket_url, "sock-tok", None).unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert_eq!(store[socket_url].api_token.as_deref(), Some("sock-tok"));
}

#[test]
fn empty_token_and_empty_session_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    assert!(set_api_token_with_paths(&paths, "https://forge.example.com", "  ", None).is_err());
    assert!(set_imported_session_with_paths(
        &paths,
        "https://forge.example.com",
        None,
        CookieJar::default()
    )
    .is_err());
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn new_store_dir_and_files_are_owner_only() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(&temp.path().join("data"));

    set_api_token_with_paths(&paths, "https://forge.example.com", "tok-1", None).unwrap();

    assert_eq!(file_mode(&paths.dir), 0o700);
    assert_eq!(file_mode(&paths.path), 0o600);
    let lock = paths.dir.join("ui-creds.json.lock");
    assert_eq!(file_mode(&lock), 0o600);
}

#[cfg(unix)]
#[test]
fn rewriting_a_world_readable_store_makes_it_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    set_ui_creds_with_paths(&paths, "https://forge.example.com", "alice", "pw").unwrap();
    std::fs::set_permissions(&paths.path, std::fs::Permissions::from_mode(0o644)).unwrap();

    set_api_token_with_paths(&paths, "https://forge.example.com", "tok-1", None).unwrap();

    assert_eq!(file_mode(&paths.path), 0o600);
}

#[cfg(unix)]
#[test]
fn repair_backup_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let paths = test_store_paths(temp.path());
    std::fs::write(
        &paths.path,
        r#"{"forge.example.com": {"baseUrl": "https://forge.example.com", "username": "alice", "password": "pw", "#,
    )
    .unwrap();
    std::fs::set_permissions(&paths.path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let store = read_creds_store_with_paths(&paths).unwrap();
    assert!(store.contains_key("forge.example.com"));

    let backups = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains(".bad."))
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(file_mode(&backups[0]), 0o600);
    assert_eq!(file_mode(&paths.path), 0o600);
}

fn test_store_paths(dir: &Path) -> StorePaths {
    StorePaths {
        dir: dir.to_path_buf(),
        path: dir.join("ui-creds.json"),
    }
}

fn test_cookie_jar(value: &str) -> CookieJar {
    CookieJar {
        saved_utc: Some("2026-01-01T00:00:00Z".to_string()),
        cookies: vec![CookieRecord {
            name: "i_like_forgejo".to_string(),
            value: value.to_string(),
            domain: "forge.example.com".to_string(),
            host_only: true,
            path: "/".to_string(),
            expires_utc: None,
            secure: true,
            http_only: true,
            same_site: Some("Lax".to_string()),
        }],
    }
}
