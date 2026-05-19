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
}
