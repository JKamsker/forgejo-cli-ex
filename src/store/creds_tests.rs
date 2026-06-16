use std::{path::Path, sync::Arc, time::Duration};

use super::{
    creds::{
        get_store_entry_with_paths, save_cookie_jar_with_paths, set_ui_creds_with_paths, CookieJar,
        CookieRecord, CredsStore, StoreEntry,
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
fn concurrent_cookie_saves_preserve_creds_and_valid_json() {
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
    assert!(entry.cookie_jar.is_some());
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
            path: "/".to_string(),
            expires_utc: None,
            secure: true,
            http_only: true,
            same_site: Some("Lax".to_string()),
        }],
    }
}
