use std::collections::BTreeMap;

use regex::Regex;

use super::{
    creds::{CredsStore, StoreEntry},
    now_rfc3339,
};

pub(super) fn repair_creds_store_from_raw(raw: &str) -> eyre::Result<CredsStore> {
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
"#;

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
