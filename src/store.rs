use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use eyre::{eyre, Context};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

pub type CredsStore = BTreeMap<String, StoreEntry>;

#[derive(Clone, Debug)]
pub struct StorePaths {
    pub dir: PathBuf,
    pub path: PathBuf,
}

fn app_data_base_dir() -> eyre::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.data_dir().to_path_buf()))
        .ok_or_else(|| eyre!("unable to locate AppData directory"))
}

pub fn ui_creds_store_paths() -> eyre::Result<StorePaths> {
    let app_data = app_data_base_dir()?;

    let dir = app_data.join("Cyborus").join("forgejo-cli").join("data");
    let path = dir.join("ui-creds.json");
    Ok(StorePaths { dir, path })
}

pub fn keys_store_paths() -> eyre::Result<StorePaths> {
    let app_data = app_data_base_dir()?;

    let dir = app_data.join("Cyborus").join("forgejo-cli").join("data");
    let path = dir.join("keys.json");
    Ok(StorePaths { dir, path })
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct KeysStore {
    #[serde(default)]
    pub hosts: BTreeMap<String, KeysHostEntry>,

    #[serde(default)]
    pub aliases: BTreeMap<String, String>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct KeysHostEntry {
    pub token: Option<String>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub fn read_keys_store() -> eyre::Result<KeysStore> {
    let store_path = keys_store_paths()?.path;
    if !store_path.is_file() {
        return Ok(KeysStore::default());
    }

    let raw = std::fs::read_to_string(&store_path)
        .wrap_err_with(|| format!("failed to read keys store at '{}'", store_path.display()))?;
    if raw.trim().is_empty() {
        return Ok(KeysStore::default());
    }

    serde_json::from_str::<KeysStore>(&raw).wrap_err_with(|| {
        format!(
            "invalid keys store JSON at '{}'. Re-run `fj auth login` to recreate.",
            store_path.display()
        )
    })
}

pub fn get_fj_api_token_for_base_url(base_url: &str) -> eyre::Result<Option<String>> {
    let keys = read_keys_store()?;
    get_fj_api_token_for_base_url_from_store(&keys, base_url)
}

fn get_fj_api_token_for_base_url_from_store(
    keys: &KeysStore,
    base_url: &str,
) -> eyre::Result<Option<String>> {
    let mut host_key = crate::target::normalize_host_key(base_url)?;
    let mut seen = std::collections::HashSet::new();

    // Follow aliases a few times to avoid accidental loops.
    for _ in 0..10 {
        if !seen.insert(host_key.clone()) {
            return Ok(None);
        }

        if let Some(entry) = keys.hosts.get(&host_key) {
            return Ok(entry.token.clone());
        }

        let Some(next) = keys.aliases.get(&host_key) else {
            return Ok(None);
        };
        host_key = crate::target::normalize_host_key(next)
            .wrap_err_with(|| format!("invalid keys.json alias target '{next}'"))?;
    }

    Ok(None)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StoreEntry {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,

    pub username: Option<String>,
    pub password: Option<String>,

    #[serde(rename = "userPass")]
    pub user_pass: Option<String>,

    #[serde(rename = "updatedUtc")]
    pub updated_utc: Option<String>,

    #[serde(rename = "cookieJar")]
    pub cookie_jar: Option<CookieJar>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CookieJar {
    #[serde(rename = "savedUtc")]
    pub saved_utc: Option<String>,

    #[serde(default)]
    pub cookies: Vec<CookieRecord>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CookieRecord {
    pub name: String,
    pub value: String,
    pub domain: String,

    #[serde(default = "default_cookie_path")]
    pub path: String,

    #[serde(rename = "expiresUtc")]
    pub expires_utc: Option<String>,

    #[serde(default)]
    pub secure: bool,

    #[serde(rename = "httpOnly", default)]
    pub http_only: bool,

    #[serde(rename = "sameSite")]
    pub same_site: Option<String>,
}

fn default_cookie_path() -> String {
    "/".to_string()
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp().to_string())
}

fn backup_path(original: &Path) -> eyre::Result<PathBuf> {
    let stamp = OffsetDateTime::now_utc()
        .format(&time::format_description::parse(
            "[year][month][day]T[hour][minute][second]Z",
        )?)
        .wrap_err("failed to format timestamp")?;
    Ok(PathBuf::from(format!(
        "{}.bad.{stamp}.json",
        original.display()
    )))
}

pub async fn read_creds_store() -> eyre::Result<CredsStore> {
    let paths = ui_creds_store_paths()?;
    let store_path = &paths.path;
    if !store_path.is_file() {
        return Ok(CredsStore::default());
    }

    #[cfg(unix)]
    let _lock = {
        let lock_path = store_path.clone();
        let lock = tokio::task::spawn_blocking(move || {
            unix_file_security::acquire_lock_blocking(&lock_path, 5)
        })
        .await
        .wrap_err("lock task panicked")??;
        unix_file_security::check_creds_permissions(store_path);
        lock
    };

    let raw = tokio::fs::read_to_string(store_path)
        .await
        .wrap_err("failed to read creds store")?;
    if raw.trim().is_empty() {
        return Ok(CredsStore::default());
    }

    match serde_json::from_str::<CredsStore>(&raw) {
        Ok(store) => Ok(store),
        Err(err) => {
            let backup = backup_path(store_path)?;
            let _ = tokio::fs::copy(store_path, &backup).await;

            let repaired = repair_creds_store_from_raw(&raw)?;
            if !repaired.is_empty() {
                eprintln!(
                    "warn: ui-creds.json was invalid JSON; backed up to '{}' and repaired by dropping cookie jars.",
                    backup.display()
                );
                // Use inner write to avoid re-entrant deadlock (Pitfall 1)
                write_creds_store_inner(&repaired, &paths).await?;
                return Ok(repaired);
            }

            Err(err).wrap_err_with(|| {
                format!(
                    "invalid creds store JSON at '{}'. Backed up to '{}'. Re-run `fj-ex auth login` (or legacy `fj-ex login`) to recreate.",
                    store_path.display(),
                    backup.display()
                )
            })
        }
    }
}

/// Inner write: no locking. Called when lock is already held (repair path)
/// or when locking is done by the caller.
async fn write_creds_store_inner(store: &CredsStore, paths: &StorePaths) -> eyre::Result<()> {
    tokio::fs::create_dir_all(&paths.dir)
        .await
        .wrap_err("failed to create creds store directory")?;

    let json = serde_json::to_vec_pretty(store).wrap_err("failed to serialize creds store")?;
    tokio::fs::write(&paths.path, json)
        .await
        .wrap_err("failed to write creds store")?;

    #[cfg(unix)]
    unix_file_security::enforce_permissions(&paths.path)?;

    Ok(())
}

pub async fn write_creds_store(store: &CredsStore) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;

    #[cfg(unix)]
    {
        let lock_path = paths.path.clone();
        let _lock = tokio::task::spawn_blocking(move || {
            unix_file_security::acquire_lock_blocking(&lock_path, 5)
        })
        .await
        .wrap_err("lock task panicked")??;

        write_creds_store_inner(store, &paths).await?;
        // _lock dropped here, releasing flock
    }

    #[cfg(not(unix))]
    {
        write_creds_store_inner(store, &paths).await?;
    }

    Ok(())
}

fn repair_creds_store_from_raw(raw: &str) -> eyre::Result<CredsStore> {
    let mut repaired = CredsStore::default();

    let host_re = Regex::new(r#""(?P<host>[^"]+)"\s*:\s*\{"#)?;
    let user_re = Regex::new(r#""username"\s*:\s*"(?P<u>[^"]*)""#)?;
    let pass_re = Regex::new(r#""password"\s*:\s*"(?P<p>[^"]*)""#)?;
    let base_re = Regex::new(r#""baseUrl"\s*:\s*"(?P<b>[^"]+)""#)?;
    let updated_re = Regex::new(r#""updatedUtc"\s*:\s*"(?P<t>[^"]+)""#)?;

    let matches: Vec<_> = host_re.captures_iter(raw).collect();
    if matches.is_empty() {
        return Ok(repaired);
    }

    for (i, caps) in matches.iter().enumerate() {
        let Some(host_key) = caps.name("host").map(|m| m.as_str()) else {
            continue;
        };
        if host_key.trim().is_empty() {
            continue;
        }

        let start = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let end = matches
            .get(i + 1)
            .and_then(|c| c.get(0).map(|m| m.start()))
            .unwrap_or_else(|| raw.len());
        if end <= start {
            continue;
        }

        let slice = &raw[start..end];
        let Some(username) = user_re
            .captures(slice)
            .and_then(|c| c.name("u").map(|m| m.as_str().to_string()))
        else {
            continue;
        };
        let Some(password) = pass_re
            .captures(slice)
            .and_then(|c| c.name("p").map(|m| m.as_str().to_string()))
        else {
            continue;
        };

        let base_url = base_re
            .captures(slice)
            .and_then(|c| c.name("b").map(|m| m.as_str().to_string()))
            .or_else(|| crate::target::normalize_base_url(host_key).ok());
        let updated_utc = updated_re
            .captures(slice)
            .and_then(|c| c.name("t").map(|m| m.as_str().to_string()))
            .or_else(|| Some(now_rfc3339()));

        repaired.insert(
            host_key.to_string(),
            StoreEntry {
                base_url,
                username: Some(username.clone()),
                password: Some(password.clone()),
                user_pass: Some(format!("{username}:{password}")),
                updated_utc,
                cookie_jar: None,
                extra: BTreeMap::default(),
            },
        );
    }

    Ok(repaired)
}

#[derive(Clone, Debug)]
pub struct StoreEntryInfo {
    pub base_url: String,
    pub host_key: String,
    pub store: CredsStore,
    pub entry: Option<StoreEntry>,
}

pub async fn get_store_entry(base_url: &str) -> eyre::Result<StoreEntryInfo> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;

    let mut store = read_creds_store().await?;

    let mut entry = store.remove(&host_key);
    let legacy_key = normalized.clone();
    let mut needs_migration = false;
    if entry.is_none() {
        if let Some(e) = store.remove(&legacy_key) {
            entry = Some(e);
            needs_migration = true;
        }
    }

    if let Some(mut e) = entry {
        if e.base_url.is_none() {
            e.base_url = Some(normalized.clone());
        }

        // Migrate legacy key (baseUrl) -> hostKey if needed.
        store.insert(host_key.clone(), e.clone());
        if needs_migration {
            write_creds_store(&store).await?;
        }

        return Ok(StoreEntryInfo {
            base_url: normalized,
            host_key,
            store,
            entry: Some(e),
        });
    }

    Ok(StoreEntryInfo {
        base_url: normalized,
        host_key,
        store,
        entry: None,
    })
}

pub async fn set_ui_creds(base_url: &str, username: &str, password: &str) -> eyre::Result<()> {
    let info = get_store_entry(base_url).await?;
    let mut store = info.store;
    let existing_cookie_jar = info.entry.as_ref().and_then(|e| e.cookie_jar.clone());

    let normalized = info.base_url;
    let host_key = info.host_key;

    let entry = StoreEntry {
        base_url: Some(normalized.clone()),
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        user_pass: Some(format!("{username}:{password}")),
        updated_utc: Some(now_rfc3339()),
        cookie_jar: existing_cookie_jar,
        extra: BTreeMap::default(),
    };

    store.insert(host_key, entry);
    write_creds_store(&store).await?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct UiCreds {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub updated_utc: Option<String>,
}

pub async fn get_ui_creds(base_url: &str) -> eyre::Result<Option<UiCreds>> {
    let info = get_store_entry(base_url).await?;
    let Some(entry) = info.entry else {
        return Ok(None);
    };

    let username = entry.username.ok_or_else(|| {
        eyre!(
            "invalid creds store entry for '{}' (missing username)",
            info.base_url
        )
    })?;
    let password = entry.password.ok_or_else(|| {
        eyre!(
            "invalid creds store entry for '{}' (missing password)",
            info.base_url
        )
    })?;

    Ok(Some(UiCreds {
        base_url: info.base_url,
        username,
        password,
        updated_utc: entry.updated_utc,
    }))
}

pub async fn clear_cookie_jar(base_url: &str) -> eyre::Result<()> {
    let info = get_store_entry(base_url).await?;
    let mut store = info.store;

    let Some(mut entry) = info.entry else {
        return Ok(());
    };

    if entry.cookie_jar.is_some() {
        entry.cookie_jar = None;
        store.insert(info.host_key, entry);
        write_creds_store(&store).await?;
    }

    Ok(())
}

pub async fn save_cookie_jar(base_url: &str, cookie_jar: CookieJar) -> eyre::Result<()> {
    let info = get_store_entry(base_url).await?;
    let mut store = info.store;

    let mut entry = info.entry.unwrap_or_default();
    entry.base_url = Some(info.base_url.clone());
    entry.cookie_jar = Some(cookie_jar);

    store.insert(info.host_key, entry);
    write_creds_store(&store).await?;
    Ok(())
}

pub async fn delete_store_entry(base_url: &str) -> eyre::Result<Option<StoreEntry>> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;

    let mut store = read_creds_store().await?;

    let mut removed: Option<StoreEntry> = None;
    if let Some(entry) = store.remove(&host_key) {
        removed = Some(entry);
    }
    if let Some(entry) = store.remove(&normalized) {
        if removed.is_none() {
            removed = Some(entry);
        }
    }

    if removed.is_some() {
        write_creds_store(&store).await?;
    }

    Ok(removed)
}

#[cfg(unix)]
mod unix_file_security {
    use eyre::Context;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// RAII guard: dropping unlocks the flock.
    #[derive(Debug)]
    pub struct LockedFile(pub std::fs::File);

    impl Drop for LockedFile {
        fn drop(&mut self) {
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    /// Acquire an exclusive flock on `path` (creating the file if needed with mode 0600).
    /// Polls with LOCK_NB every 500ms. Returns error after `timeout_secs` seconds.
    pub fn acquire_lock_blocking(path: &Path, timeout_secs: u64) -> eyre::Result<LockedFile> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .wrap_err_with(|| format!("cannot open credential file '{}'", path.display()))?;

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

        loop {
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if ret == 0 {
                return Ok(LockedFile(file));
            }
            if std::time::Instant::now() >= deadline {
                return Err(eyre::eyre!(
                    "ui-creds.json locked by another process (waited {}s). \
                     Retry or check for stuck fj-ex processes.",
                    timeout_secs
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    /// Warn to stderr if permissions are too open. Non-blocking.
    pub fn check_creds_permissions(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                eprintln!(
                    "warn: {} has mode {:04o}, expected 0600. Run `fj-ex auth login` to fix.",
                    path.display(),
                    mode
                );
            }
        }
    }

    /// Enforce 0600 on every write. Uses set_permissions after write to override any umask.
    pub fn enforce_permissions(path: &Path) -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms).wrap_err_with(|| {
            format!(
                "failed to set 0600 permissions on '{}'. Run `fj-ex auth login` to recreate.",
                path.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_drops_cookie_jars_and_keeps_creds() {
        let raw = r#"
{
  "forge.example.com": {
    "baseUrl": "https://forge.example.com",
    "username": "alice",
    "password": "secret",
    "updatedUtc": "2020-01-01T00:00:00Z",
    "cookieJar": { "cookies": [ { "name": "a" } ] }
  },
  "other": {
    "username": "bob",
    "password": "hunter2"
  }
"#; // intentionally unterminated/invalid JSON

        let repaired = repair_creds_store_from_raw(raw).unwrap();
        assert!(repaired.contains_key("forge.example.com"));
        assert!(repaired.contains_key("other"));
        assert!(repaired["forge.example.com"].cookie_jar.is_none());
        assert_eq!(
            repaired["forge.example.com"].base_url.as_deref(),
            Some("https://forge.example.com")
        );
        assert_eq!(repaired["other"].base_url.as_deref(), Some("https://other"));
    }

    #[test]
    fn fj_keys_store_token_lookup_by_normalized_host_key() {
        let raw = r#"
{
  "hosts": {
    "forge.example.com:3000": { "token": "abc" }
  }
}
"#;

        let keys: KeysStore = serde_json::from_str(raw).unwrap();
        let token =
            get_fj_api_token_for_base_url_from_store(&keys, "https://forge.example.com:3000")
                .unwrap();
        assert_eq!(token.as_deref(), Some("abc"));
    }

    #[cfg(unix)]
    #[test]
    fn write_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-creds.json");

        // Write a file then enforce permissions
        std::fs::write(&path, b"{}").unwrap();
        super::unix_file_security::enforce_permissions(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {:04o}", mode);
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_resets_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-creds.json");

        // Create with 0644
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // Enforce should reset to 0600
        super::unix_file_security::enforce_permissions(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected mode 0600 after overwrite, got {:04o}",
            mode
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_timeout_error_message() {
        use std::os::unix::io::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-lock.json");
        std::fs::write(&path, b"{}").unwrap();

        // Hold a lock in the current thread via raw flock
        let file = std::fs::File::open(&path).unwrap();
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(ret, 0, "failed to acquire initial lock");

        // Attempt acquire in another thread with short timeout
        let path_clone = path.clone();
        let handle = std::thread::spawn(move || {
            super::unix_file_security::acquire_lock_blocking(&path_clone, 1)
        });

        let result = handle.join().unwrap();
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("locked by another process"),
            "error should mention 'locked by another process', got: {err_msg}"
        );
        assert!(
            err_msg.contains("Retry or check for stuck fj-ex processes"),
            "error should mention fix action, got: {err_msg}"
        );

        // Release lock
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }

    #[cfg(unix)]
    #[test]
    fn read_warns_on_open_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-creds.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // This should print a warning — we verify it doesn't panic
        super::unix_file_security::check_creds_permissions(&path);

        // Verify with 0600 it does not warn (no way to assert no stderr, but no panic)
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        super::unix_file_security::check_creds_permissions(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_writes_no_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join("ui-creds.json");

        let path = creds_path.clone();
        let mut handles = vec![];
        for i in 0..4 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                let lock = super::unix_file_security::acquire_lock_blocking(&p, 5)
                    .expect("failed to acquire lock");
                // Write valid JSON while holding lock
                let content = format!("{{\"thread\": {}}}", i);
                std::fs::write(&p, content.as_bytes()).expect("failed to write");
                super::unix_file_security::enforce_permissions(&p)
                    .expect("failed to set perms");
                drop(lock);
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }

        // File must be valid JSON after all writes
        let contents = std::fs::read_to_string(&creds_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents)
            .expect("credential file is not valid JSON after concurrent writes");
        assert!(parsed.get("thread").is_some());
    }

    #[test]
    fn fj_keys_store_token_lookup_follows_aliases() {
        let raw = r#"
{
  "hosts": {
    "forge.example.com": { "token": "real-token" }
  },
  "aliases": {
    "alias.example.com": "forge.example.com"
  }
}
"#;

        let keys: KeysStore = serde_json::from_str(raw).unwrap();
        let token =
            get_fj_api_token_for_base_url_from_store(&keys, "https://alias.example.com").unwrap();
        assert_eq!(token.as_deref(), Some("real-token"));
    }
}
