use std::collections::BTreeMap;

use eyre::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::keys_store_paths;

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

fn read_keys_store() -> eyre::Result<KeysStore> {
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
    let mut lookup_key = token_lookup_key(base_url)?;
    let mut seen = std::collections::HashSet::new();

    // Follow aliases a few times to avoid accidental loops.
    for _ in 0..10 {
        if !seen.insert(lookup_key.clone()) {
            return Ok(None);
        }

        if let Some(entry) = find_host_entry(keys, &lookup_key)? {
            return Ok(entry.token.clone());
        }

        let Some(next) = find_alias_target(keys, &lookup_key)? else {
            return Ok(None);
        };
        lookup_key = token_lookup_key(next)
            .wrap_err_with(|| format!("invalid keys.json alias target '{next}'"))?;
    }

    Ok(None)
}

fn find_host_entry<'a>(
    keys: &'a KeysStore,
    lookup_key: &str,
) -> eyre::Result<Option<&'a KeysHostEntry>> {
    if let Some(entry) = keys.hosts.get(lookup_key) {
        return Ok(Some(entry));
    }

    for (raw_key, entry) in &keys.hosts {
        if token_lookup_key(raw_key).is_ok_and(|key| key == lookup_key) {
            return Ok(Some(entry));
        }
    }

    Ok(None)
}

fn find_alias_target<'a>(keys: &'a KeysStore, lookup_key: &str) -> eyre::Result<Option<&'a str>> {
    if let Some(target) = keys.aliases.get(lookup_key) {
        return Ok(Some(target));
    }

    for (raw_key, target) in &keys.aliases {
        if token_lookup_key(raw_key).is_ok_and(|key| key == lookup_key) {
            return Ok(Some(target));
        }
    }

    Ok(None)
}

fn token_lookup_key(base_url: &str) -> eyre::Result<String> {
    let normalized = crate::target::normalize_base_url(base_url)?;
    if crate::target::normalized_base_url_has_path(&normalized) {
        return Ok(normalized);
    }

    crate::target::normalize_host_key(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn fj_keys_store_token_lookup_uses_path_specific_key() {
        let raw = r#"
{
  "hosts": {
    "forge.example.com": { "token": "root-token" },
    "https://forge.example.com/gitea": { "token": "gitea-token" }
  }
}
"#;

        let keys: KeysStore = serde_json::from_str(raw).unwrap();
        let token =
            get_fj_api_token_for_base_url_from_store(&keys, "https://forge.example.com/gitea")
                .unwrap();
        assert_eq!(token.as_deref(), Some("gitea-token"));
    }

    #[test]
    fn fj_keys_store_path_token_lookup_does_not_fall_back_to_host_key() {
        let raw = r#"
{
  "hosts": {
    "forge.example.com": { "token": "root-token" }
  }
}
"#;

        let keys: KeysStore = serde_json::from_str(raw).unwrap();
        let token =
            get_fj_api_token_for_base_url_from_store(&keys, "https://forge.example.com/gitea")
                .unwrap();
        assert_eq!(token, None);
    }

    #[test]
    fn fj_keys_store_path_token_lookup_follows_path_aliases() {
        let raw = r#"
{
  "hosts": {
    "https://forge.example.com/gitea": { "token": "gitea-token" }
  },
  "aliases": {
    "https://alias.example.com/gitea": "https://forge.example.com/gitea"
  }
}
"#;

        let keys: KeysStore = serde_json::from_str(raw).unwrap();
        let token =
            get_fj_api_token_for_base_url_from_store(&keys, "https://alias.example.com/gitea")
                .unwrap();
        assert_eq!(token.as_deref(), Some("gitea-token"));
    }
}
