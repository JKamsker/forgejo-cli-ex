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

pub fn ui_creds_store_paths() -> eyre::Result<StorePaths> {
    let app_data = std::env::var_os("APPDATA")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.data_dir().to_path_buf()))
        .ok_or_else(|| eyre!("unable to locate AppData directory"))?;

    let dir = app_data.join("Cyborus").join("forgejo-cli").join("data");
    let path = dir.join("ui-creds.json");
    Ok(StorePaths { dir, path })
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
    let store_path = ui_creds_store_paths()?.path;
    if !store_path.is_file() {
        return Ok(CredsStore::default());
    }

    let raw = tokio::fs::read_to_string(&store_path)
        .await
        .wrap_err("failed to read creds store")?;
    if raw.trim().is_empty() {
        return Ok(CredsStore::default());
    }

    match serde_json::from_str::<CredsStore>(&raw) {
        Ok(store) => Ok(store),
        Err(err) => {
            let backup = backup_path(&store_path)?;
            let _ = tokio::fs::copy(&store_path, &backup).await;

            let repaired = repair_creds_store_from_raw(&raw)?;
            if !repaired.is_empty() {
                eprintln!(
                    "warn: ui-creds.json was invalid JSON; backed up to '{}' and repaired by dropping cookie jars.",
                    backup.display()
                );
                write_creds_store(&repaired).await?;
                return Ok(repaired);
            }

            Err(err).wrap_err_with(|| {
                format!(
                    "invalid creds store JSON at '{}'. Backed up to '{}'. Re-run `fj-ex login` to recreate.",
                    store_path.display(),
                    backup.display()
                )
            })
        }
    }
}

pub async fn write_creds_store(store: &CredsStore) -> eyre::Result<()> {
    let paths = ui_creds_store_paths()?;
    tokio::fs::create_dir_all(&paths.dir)
        .await
        .wrap_err("failed to create creds store directory")?;

    let json = serde_json::to_vec_pretty(store).wrap_err("failed to serialize creds store")?;
    tokio::fs::write(&paths.path, json)
        .await
        .wrap_err("failed to write creds store")?;
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
}
