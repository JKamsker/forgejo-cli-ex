use std::collections::BTreeMap;

use eyre::eyre;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{file, lock::StoreLockMode, now_rfc3339, ui_creds_store_paths, StorePaths};

pub type CredsStore = BTreeMap<String, StoreEntry>;

/// How the UI side of a stored login is maintained.
///
/// Entries written before this field existed have no `kind`; they are
/// password logins when both username and password are present, and are
/// otherwise treated as broken and repaired away.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    /// Username + password; the cookie session is refreshed by re-login.
    Password,
    /// A browser session cookie imported with `--session-cookie-stdin`.
    /// It cannot be refreshed: once it expires it must be re-imported.
    Session,
    /// API token only; there is no UI session for this host.
    Token,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Session => "session",
            Self::Token => "token",
        }
    }
}

/// JSON key of the credential kind. A `kind` value this build does not know
/// (from a newer version, or a hand edit) is kept in [`StoreEntry::extra`]
/// under this key, so the entry survives reads and rewrites unchanged instead
/// of making the whole store invalid.
const KIND_KEY: &str = "kind";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(from = "RawStoreEntry", into = "RawStoreEntry")]
pub struct StoreEntry {
    pub base_url: Option<String>,

    /// Credential kind of this entry (absent in stores written by older versions).
    pub kind: Option<CredentialKind>,

    pub username: Option<String>,
    pub password: Option<String>,

    pub user_pass: Option<String>,

    /// Forgejo/Gitea API token stored by `fj-ex auth login --token[-stdin]`.
    pub api_token: Option<String>,

    pub updated_utc: Option<String>,

    pub cookie_jar: Option<CookieJar>,

    pub extra: BTreeMap<String, Value>,
}

/// On-disk shape of [`StoreEntry`].
#[derive(Serialize, Deserialize)]
struct RawStoreEntry {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<Value>,

    username: Option<String>,
    password: Option<String>,

    #[serde(rename = "userPass")]
    user_pass: Option<String>,

    #[serde(rename = "apiToken", default, skip_serializing_if = "Option::is_none")]
    api_token: Option<String>,

    #[serde(rename = "updatedUtc")]
    updated_utc: Option<String>,

    #[serde(rename = "cookieJar")]
    cookie_jar: Option<CookieJar>,

    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// Parses a stored `kind` value, ignoring case and surrounding whitespace.
fn parse_credential_kind(value: &Value) -> Option<CredentialKind> {
    match value.as_str()?.trim().to_ascii_lowercase().as_str() {
        "password" => Some(CredentialKind::Password),
        "session" => Some(CredentialKind::Session),
        "token" => Some(CredentialKind::Token),
        _ => None,
    }
}

impl From<RawStoreEntry> for StoreEntry {
    fn from(raw: RawStoreEntry) -> Self {
        let mut extra = raw.extra;
        let kind = match raw.kind {
            None | Some(Value::Null) => None,
            Some(value) => {
                let kind = parse_credential_kind(&value);
                if kind.is_none() {
                    extra.insert(KIND_KEY.to_string(), value);
                }
                kind
            }
        };
        Self {
            base_url: raw.base_url,
            kind,
            username: raw.username,
            password: raw.password,
            user_pass: raw.user_pass,
            api_token: raw.api_token,
            updated_utc: raw.updated_utc,
            cookie_jar: raw.cookie_jar,
            extra,
        }
    }
}

impl From<StoreEntry> for RawStoreEntry {
    fn from(entry: StoreEntry) -> Self {
        let mut extra = entry.extra;
        let unrecognized_kind = extra.remove(KIND_KEY);
        let kind = match entry.kind {
            Some(kind) => Some(Value::String(kind.as_str().to_string())),
            None => unrecognized_kind,
        };
        Self {
            base_url: entry.base_url,
            kind,
            username: entry.username,
            password: entry.password,
            user_pass: entry.user_pass,
            api_token: entry.api_token,
            updated_utc: entry.updated_utc,
            cookie_jar: entry.cookie_jar,
            extra,
        }
    }
}

impl StoreEntry {
    /// Whether the stored `kind` is one this build does not know.
    pub(super) fn has_unrecognized_kind(&self) -> bool {
        self.kind.is_none() && self.extra.contains_key(KIND_KEY)
    }

    fn has_password(&self) -> bool {
        nonempty(self.username.as_deref()) && nonempty(self.password.as_deref())
    }

    fn has_session_cookies(&self) -> bool {
        self.cookie_jar
            .as_ref()
            .is_some_and(|jar| !jar.cookies.is_empty())
    }

    pub fn has_api_token(&self) -> bool {
        nonempty(self.api_token.as_deref())
    }

    /// The effective credential kind, resolving legacy entries without a `kind`.
    pub fn credential_kind(&self) -> Option<CredentialKind> {
        match self.kind {
            Some(kind) => Some(kind),
            // Do not guess what an unknown (newer) kind means.
            None if self.has_unrecognized_kind() => None,
            None if self.has_password() => Some(CredentialKind::Password),
            None if self.has_api_token() => Some(CredentialKind::Token),
            None => None,
        }
    }

    /// The UI login method this entry provides, if any.
    pub fn ui_login_kind(&self) -> Option<CredentialKind> {
        match self.credential_kind()? {
            CredentialKind::Password if self.has_password() => Some(CredentialKind::Password),
            CredentialKind::Session if self.has_session_cookies() => Some(CredentialKind::Session),
            _ => None,
        }
    }

    /// Whether the entry still holds at least one usable credential.
    ///
    /// Cookie jars on their own are only meaningful for imported sessions;
    /// a legacy cookie-only entry (no kind, no password) is broken. Entries
    /// of an unrecognized kind are kept for the version that wrote them.
    pub(super) fn is_usable(&self) -> bool {
        self.has_api_token() || self.ui_login_kind().is_some() || self.has_unrecognized_kind()
    }

    /// Short human-readable description such as `password` or `session + token`.
    pub fn credential_summary(&self) -> String {
        match (self.ui_login_kind(), self.has_api_token()) {
            (Some(kind), true) => format!("{} + token", kind.as_str()),
            (Some(kind), false) => kind.as_str().to_string(),
            (None, true) => "token".to_string(),
            (None, false) => "none".to_string(),
        }
    }
}

fn nonempty(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|v| !v.is_empty())
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

    #[serde(rename = "hostOnly", default)]
    pub host_only: bool,

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

/// Result of storing an API token for a host.
#[derive(Clone, Debug)]
pub struct StoredTokenOutcome {
    /// The UI login kept alongside the token (password or imported session).
    pub kept_ui_login: Option<CredentialKind>,
    /// Username of the existing UI login when it differs from the token's user.
    pub ui_username_mismatch: Option<String>,
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

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let existing = take_store_entry(store, &normalized, &host_key);
        let existing_cookie_jar = existing.as_ref().and_then(|e| e.cookie_jar.clone());
        let existing_token = existing.as_ref().and_then(|e| e.api_token.clone());
        let extra = existing.map(|e| e.extra).unwrap_or_default();

        insert_entry(
            store,
            &normalized,
            &host_key,
            StoreEntry {
                base_url: Some(normalized.clone()),
                kind: Some(CredentialKind::Password),
                username: Some(username.to_string()),
                password: Some(password.to_string()),
                user_pass: Some(format!("{username}:{password}")),
                api_token: existing_token,
                updated_utc: Some(now_rfc3339()),
                cookie_jar: existing_cookie_jar,
                extra,
            },
        );
        Ok(((), true))
    })?;
    Ok(())
}

/// Store an imported browser session. Replaces a stored password login (the
/// session is now the UI credential) but keeps a stored API token.
pub async fn set_imported_session(
    base_url: &str,
    username: Option<&str>,
    cookie_jar: CookieJar,
) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;
    set_imported_session_with_paths(&paths, base_url, username, cookie_jar)
}

pub(super) fn set_imported_session_with_paths(
    paths: &StorePaths,
    base_url: &str,
    username: Option<&str>,
    cookie_jar: CookieJar,
) -> eyre::Result<()> {
    if cookie_jar.cookies.is_empty() {
        return Err(eyre!("imported session must contain at least one cookie"));
    }

    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let existing = take_store_entry(store, &normalized, &host_key);
        let existing_token = existing.as_ref().and_then(|e| e.api_token.clone());
        let existing_username = existing.as_ref().and_then(|e| e.username.clone());
        let extra = existing.map(|e| e.extra).unwrap_or_default();

        insert_entry(
            store,
            &normalized,
            &host_key,
            StoreEntry {
                base_url: Some(normalized.clone()),
                kind: Some(CredentialKind::Session),
                username: username
                    .map(ToOwned::to_owned)
                    .filter(|u| !u.trim().is_empty())
                    .or(existing_username),
                password: None,
                user_pass: None,
                api_token: existing_token,
                updated_utc: Some(now_rfc3339()),
                cookie_jar: Some(cookie_jar),
                extra,
            },
        );
        Ok(((), true))
    })?;
    Ok(())
}

/// Store an API token. An existing UI login (password or imported session) is kept.
pub async fn set_api_token(
    base_url: &str,
    token: &str,
    username: Option<&str>,
) -> eyre::Result<StoredTokenOutcome> {
    let paths = ui_creds_store_paths()?;
    set_api_token_with_paths(&paths, base_url, token, username)
}

pub(super) fn set_api_token_with_paths(
    paths: &StorePaths,
    base_url: &str,
    token: &str,
    username: Option<&str>,
) -> eyre::Result<StoredTokenOutcome> {
    let token = token.trim();
    if token.is_empty() {
        return Err(eyre!("API token must not be empty"));
    }

    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;
    let username = username.map(str::trim).filter(|u| !u.is_empty());

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let mut entry = take_store_entry(store, &normalized, &host_key)
            .filter(StoreEntry::is_usable)
            .unwrap_or_default();

        let kept_ui_login = entry.ui_login_kind();
        let ui_username_mismatch = match (kept_ui_login, entry.username.as_deref(), username) {
            (Some(_), Some(existing), Some(token_user)) if existing != token_user => {
                Some(existing.to_string())
            }
            _ => None,
        };

        if kept_ui_login.is_none() {
            entry.kind = Some(CredentialKind::Token);
            entry.password = None;
            entry.user_pass = None;
            entry.cookie_jar = None;
            entry.username = username.map(ToOwned::to_owned);
        } else {
            // Pin the kind so the entry is no longer treated as legacy.
            entry.kind = kept_ui_login;
            if entry.username.is_none() {
                entry.username = username.map(ToOwned::to_owned);
            }
        }
        entry.base_url = Some(normalized.clone());
        entry.api_token = Some(token.to_string());
        entry.updated_utc = Some(now_rfc3339());

        insert_entry(store, &normalized, &host_key, entry);
        Ok((
            StoredTokenOutcome {
                kept_ui_login,
                ui_username_mismatch,
            },
            true,
        ))
    })?
    .ok_or_else(|| eyre!("failed to update creds store"))
}

/// The API token stored by `fj-ex auth login --token[-stdin]` for this base URL.
pub async fn get_stored_api_token(base_url: &str) -> eyre::Result<Option<String>> {
    let info = get_store_entry(base_url).await?;
    Ok(info
        .entry
        .and_then(|e| e.api_token)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty()))
}

/// Password credentials for basic auth and automatic UI re-login.
///
/// Returns `None` for hosts without a stored password (imported sessions and
/// token-only logins).
pub async fn get_ui_creds(base_url: &str) -> eyre::Result<Option<UiCreds>> {
    let info = get_store_entry(base_url).await?;
    let Some(entry) = info.entry else {
        return Ok(None);
    };

    if entry.ui_login_kind() != Some(CredentialKind::Password) {
        return Ok(None);
    }

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

    file::update_creds_store(paths, StoreLockMode::Required, |store| {
        let Some(mut entry) = take_store_entry(store, &normalized, &host_key) else {
            return Ok(((), false));
        };

        if !entry.is_usable() {
            return Ok(((), true));
        }

        let was_session = entry.ui_login_kind() == Some(CredentialKind::Session);
        entry.base_url = Some(normalized.clone());
        entry.cookie_jar = None;
        if was_session {
            // Clearing an imported session's cookies removes the session itself.
            if !entry.has_api_token() {
                return Ok(((), true));
            }
            entry.kind = Some(CredentialKind::Token);
        }
        insert_entry(store, &normalized, &host_key, entry);
        Ok(((), true))
    })?;
    Ok(())
}

pub async fn save_cookie_jar(base_url: &str, cookie_jar: CookieJar) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;
    save_cookie_jar_with_paths_and_mode(&paths, base_url, cookie_jar, StoreLockMode::Optional)
}

pub async fn save_cookie_jar_required(base_url: &str, cookie_jar: CookieJar) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;
    save_cookie_jar_with_paths_and_mode(&paths, base_url, cookie_jar, StoreLockMode::Required)
}

#[cfg(test)]
pub(super) fn save_cookie_jar_with_paths(
    paths: &StorePaths,
    base_url: &str,
    cookie_jar: CookieJar,
) -> eyre::Result<()> {
    save_cookie_jar_with_paths_and_mode(paths, base_url, cookie_jar, StoreLockMode::Optional)
}

#[cfg(test)]
pub(super) fn save_cookie_jar_required_with_paths(
    paths: &StorePaths,
    base_url: &str,
    cookie_jar: CookieJar,
) -> eyre::Result<()> {
    save_cookie_jar_with_paths_and_mode(paths, base_url, cookie_jar, StoreLockMode::Required)
}

fn save_cookie_jar_with_paths_and_mode(
    paths: &StorePaths,
    base_url: &str,
    cookie_jar: CookieJar,
    lock_mode: StoreLockMode,
) -> eyre::Result<()> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    let host_key = crate::target::normalize_host_key(&normalized)?;

    file::update_creds_store(paths, lock_mode, |store| {
        let Some(mut entry) = take_store_entry(store, &normalized, &host_key) else {
            return Ok(((), false));
        };

        if !entry.is_usable() {
            return Ok(((), true));
        }

        entry.base_url = Some(normalized.clone());
        // Cookies are only meaningful for entries with a UI login. Token-only
        // entries are put back unchanged.
        if entry.ui_login_kind().is_some() {
            if entry.kind.is_none() {
                entry.kind = entry.ui_login_kind();
            }
            entry.cookie_jar = Some(cookie_jar);
        }
        insert_entry(store, &normalized, &host_key, entry);
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

/// Drop entries without any usable credential (genuinely broken entries,
/// such as legacy cookie-only entries). Imported sessions and token-only
/// logins are kept because their `kind` says what they are.
pub(super) fn remove_unusable_entries(store: &mut CredsStore) -> usize {
    let before = store.len();
    store.retain(|_, entry| entry.is_usable());
    before - store.len()
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

/// Insert an entry under its canonical key. If that key is already used by a
/// login for a different base URL (e.g. `http://` vs `https://` on the same
/// host), fall back to the full normalized base URL as key so the other login
/// is not overwritten.
fn insert_entry(store: &mut CredsStore, normalized: &str, host_key: &str, entry: StoreEntry) {
    let key = store_key_for_base_url(normalized, host_key);
    let key = match store.get(&key) {
        Some(other) if other.base_url.is_some() && !entry_matches_base_url(other, normalized) => {
            normalized.to_string()
        }
        _ => key,
    };
    store.insert(key, entry);
}

fn store_key_for_base_url(normalized: &str, host_key: &str) -> String {
    if crate::target::normalized_base_url_has_path(normalized) {
        normalized.to_string()
    } else {
        host_key.to_string()
    }
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
