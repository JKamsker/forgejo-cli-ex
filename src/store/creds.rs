use std::collections::BTreeMap;

use eyre::eyre;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{file, lock::StoreLockMode, now_rfc3339, ui_creds_store_paths, StorePaths};

pub type CredsStore = BTreeMap<String, StoreEntry>;

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

#[derive(Clone, Debug)]
pub struct StoreEntryInfo {
    pub base_url: String,
    pub entry: Option<StoreEntry>,
}

#[derive(Clone, Debug)]
pub struct UiCreds {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub updated_utc: Option<String>,
}

pub async fn read_creds_store() -> eyre::Result<CredsStore> {
    let paths = ui_creds_store_paths()?;
    file::read_creds_store_with_paths(&paths)
}

pub async fn get_store_entry(base_url: &str) -> eyre::Result<StoreEntryInfo> {
    let paths = ui_creds_store_paths()?;
    get_store_entry_with_paths(&paths, base_url)
}

pub(super) fn get_store_entry_with_paths(
    paths: &StorePaths,
    base_url: &str,
) -> eyre::Result<StoreEntryInfo> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;

    let store = file::read_creds_store_with_paths(paths)?;
    let entry = find_store_entry(&store, &normalized, &host_key);

    Ok(StoreEntryInfo {
        base_url: normalized,
        entry,
    })
}

pub async fn set_ui_creds(base_url: &str, username: &str, password: &str) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;
    set_ui_creds_with_paths(&paths, base_url, username, password)
}

pub(super) fn set_ui_creds_with_paths(
    paths: &StorePaths,
    base_url: &str,
    username: &str,
    password: &str,
) -> eyre::Result<()> {
    if username.trim().is_empty() || password.trim().is_empty() {
        return Err(eyre!("username/password must not be empty"));
    }

    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;
    let store_key = store_key_for_base_url(&normalized, &host_key);

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let existing_cookie_jar =
            take_store_entry(store, &normalized, &host_key).and_then(|e| e.cookie_jar.clone());

        store.insert(
            store_key,
            StoreEntry {
                base_url: Some(normalized.clone()),
                username: Some(username.to_string()),
                password: Some(password.to_string()),
                user_pass: Some(format!("{username}:{password}")),
                updated_utc: Some(now_rfc3339()),
                cookie_jar: existing_cookie_jar,
                extra: BTreeMap::default(),
            },
        );
        Ok(((), true))
    })?;
    Ok(())
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
    let paths = ui_creds_store_paths()?;
    clear_cookie_jar_with_paths(&paths, base_url)
}

pub(super) fn clear_cookie_jar_with_paths(paths: &StorePaths, base_url: &str) -> eyre::Result<()> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;
    let store_key = store_key_for_base_url(&normalized, &host_key);

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let Some(mut entry) = take_store_entry(store, &normalized, &host_key) else {
            return Ok(((), false));
        };

        if !entry_has_complete_creds(&entry) {
            return Ok(((), true));
        }

        entry.base_url = Some(normalized.clone());
        entry.cookie_jar = None;
        store.insert(store_key, entry);
        Ok(((), true))
    })?;
    Ok(())
}

pub async fn save_cookie_jar(base_url: &str, cookie_jar: CookieJar) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;
    save_cookie_jar_with_paths(&paths, base_url, cookie_jar)
}

pub(super) fn save_cookie_jar_with_paths(
    paths: &StorePaths,
    base_url: &str,
    cookie_jar: CookieJar,
) -> eyre::Result<()> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;
    let store_key = store_key_for_base_url(&normalized, &host_key);

    file::update_creds_store(paths, StoreLockMode::Optional, |store| {
        let Some(mut entry) = take_store_entry(store, &normalized, &host_key) else {
            return Ok(((), false));
        };

        if !entry_has_complete_creds(&entry) {
            return Ok(((), true));
        }

        entry.base_url = Some(normalized.clone());
        entry.cookie_jar = Some(cookie_jar);
        store.insert(store_key, entry);
        Ok(((), true))
    })?;
    Ok(())
}

pub async fn delete_store_entry(base_url: &str) -> eyre::Result<Option<StoreEntry>> {
    let paths = ui_creds_store_paths()?;
    delete_store_entry_with_paths(&paths, base_url)
}

pub(super) fn delete_store_entry_with_paths(
    paths: &StorePaths,
    base_url: &str,
) -> eyre::Result<Option<StoreEntry>> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let removed = take_store_entry(store, &normalized, &host_key);
        let changed = removed.is_some();
        Ok((removed, changed))
    })
    .map(|result| result.flatten())
}

pub(super) fn remove_entries_without_complete_creds(store: &mut CredsStore) -> usize {
    let before = store.len();
    store.retain(|_, entry| entry_has_complete_creds(entry));
    before - store.len()
}

fn entry_has_complete_creds(entry: &StoreEntry) -> bool {
    entry
        .username
        .as_deref()
        .map(str::trim)
        .is_some_and(|v| !v.is_empty())
        && entry
            .password
            .as_deref()
            .map(str::trim)
            .is_some_and(|v| !v.is_empty())
}

fn find_store_entry(store: &CredsStore, normalized: &str, host_key: &str) -> Option<StoreEntry> {
    for key in lookup_keys(normalized, host_key) {
        let Some(entry) = store.get(&key) else {
            continue;
        };
        if entry_matches_base_url(entry, normalized) {
            return Some(entry_with_default_base_url(entry.clone(), normalized));
        }
    }

    for entry in store.values() {
        if entry_matches_base_url(entry, normalized) {
            return Some(entry_with_default_base_url(entry.clone(), normalized));
        }
    }

    for key in lookup_keys(normalized, host_key) {
        let Some(entry) = store.get(&key) else {
            continue;
        };
        if entry.base_url.is_none() {
            return Some(entry_with_default_base_url(entry.clone(), normalized));
        }
    }

    None
}

fn take_store_entry(
    store: &mut CredsStore,
    normalized: &str,
    host_key: &str,
) -> Option<StoreEntry> {
    let mut keys = Vec::new();
    for key in lookup_keys(normalized, host_key) {
        if store
            .get(&key)
            .is_some_and(|entry| entry_matches_base_url(entry, normalized))
        {
            keys.push(key);
        }
    }
    for (key, entry) in store.iter() {
        if entry_matches_base_url(entry, normalized) && !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    for key in lookup_keys(normalized, host_key) {
        if store
            .get(&key)
            .is_some_and(|entry| entry.base_url.is_none())
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }

    let mut first = None;
    for key in keys {
        if first.is_none() {
            first = store.remove(&key);
        } else {
            let _ = store.remove(&key);
        }
    }
    first
}

fn store_key_for_base_url(normalized: &str, host_key: &str) -> String {
    if normalized_has_path(normalized) {
        normalized.to_string()
    } else {
        host_key.to_string()
    }
}

fn normalized_has_path(normalized: &str) -> bool {
    url::Url::parse(normalized).is_ok_and(|url| {
        let path = url.path().trim_end_matches('/');
        !path.is_empty() && path != "/"
    })
}

fn lookup_keys(normalized: &str, host_key: &str) -> Vec<String> {
    let mut keys = Vec::new();
    push_unique(&mut keys, normalized.to_string());
    push_unique(&mut keys, host_key.to_string());
    if let Some(origin) = origin_base_url(normalized) {
        push_unique(&mut keys, origin);
    }
    keys
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn origin_base_url(normalized: &str) -> Option<String> {
    let url = url::Url::parse(normalized).ok()?;
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{}{}", url.scheme(), host, port))
}

fn entry_matches_base_url(entry: &StoreEntry, normalized: &str) -> bool {
    entry
        .base_url
        .as_deref()
        .and_then(|base| crate::target::normalize_base_url(base).ok())
        .is_some_and(|base| base == normalized)
}

fn entry_with_default_base_url(mut entry: StoreEntry, normalized: &str) -> StoreEntry {
    if entry.base_url.is_none() {
        entry.base_url = Some(normalized.to_string());
    }
    entry
}
