use cookie_store::{Cookie, CookieDomain, CookieExpiration};
use eyre::Context;
use reqwest_cookie_store::CookieStoreMutex;
use time::OffsetDateTime;
use url::Url;

use crate::store;

pub(crate) fn load_cookie_jar_into_store(
    cookie_store: &CookieStoreMutex,
    jar: &store::CookieJar,
) -> eyre::Result<()> {
    let mut guard = cookie_store.lock().unwrap();

    for record in &jar.cookies {
        let domain = record.domain.trim().trim_start_matches('.');
        if domain.is_empty() {
            continue;
        }

        let request_url = match Url::parse(&format!("https://{domain}/")) {
            Ok(u) => u,
            Err(_) => continue,
        };

        let mut builder =
            cookie_store::RawCookie::build((record.name.clone(), record.value.clone()))
                .path(record.path.clone())
                .secure(record.secure)
                .http_only(record.http_only);

        if !record.host_only {
            builder = builder.domain(domain.to_string());
        }

        if let Some(same_site) = record.same_site.as_deref() {
            let same_site = match same_site.to_ascii_lowercase().as_str() {
                "lax" => Some(cookie::SameSite::Lax),
                "strict" => Some(cookie::SameSite::Strict),
                "none" => Some(cookie::SameSite::None),
                _ => None,
            };
            if let Some(same_site) = same_site {
                builder = builder.same_site(same_site);
            }
        }

        if let Some(expires) = record.expires_utc.as_deref() {
            if let Ok(dt) =
                time::OffsetDateTime::parse(expires, &time::format_description::well_known::Rfc3339)
            {
                builder = builder.expires(dt);
            }
        }

        let raw_cookie = builder.build();
        let cookie = Cookie::try_from_raw_cookie(&raw_cookie, &request_url)
            .wrap_err("failed to reconstruct cookie")?
            .into_owned();
        let _ = guard.insert(cookie, &request_url);
    }

    Ok(())
}

pub(crate) fn cookie_jar_from_store(
    cookie_store: &CookieStoreMutex,
) -> eyre::Result<store::CookieJar> {
    let guard = cookie_store.lock().unwrap();
    let now = OffsetDateTime::now_utc();

    let mut cookies = Vec::new();
    for c in guard.iter_any() {
        if c.is_expired() {
            continue;
        }

        let (domain, host_only) = match &c.domain {
            CookieDomain::HostOnly(domain) => (domain.clone(), true),
            CookieDomain::Suffix(domain) => (domain.clone(), false),
            CookieDomain::Empty | CookieDomain::NotPresent => continue,
        };

        let expires_utc = match c.expires {
            CookieExpiration::AtUtc(dt) => Some(
                dt.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            ),
            CookieExpiration::SessionEnd => None,
        };

        cookies.push(store::CookieRecord {
            name: c.name().to_string(),
            value: c.value().to_string(),
            domain,
            host_only,
            path: c.path.to_string(),
            expires_utc,
            secure: c.secure().unwrap_or(false),
            http_only: c.http_only().unwrap_or(false),
            same_site: c.same_site().map(|s| s.to_string()),
        });
    }

    Ok(store::CookieJar {
        saved_utc: Some(
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        ),
        cookies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cookie_store::CookieStore;

    #[test]
    fn suffix_cookie_records_roundtrip_into_cookie_store() {
        let jar = store::CookieJar {
            saved_utc: Some("2020-01-01T00:00:00Z".to_string()),
            cookies: vec![store::CookieRecord {
                name: "a".to_string(),
                value: "b".to_string(),
                domain: "forge.example.com".to_string(),
                host_only: false,
                path: "/".to_string(),
                expires_utc: None,
                secure: true,
                http_only: true,
                same_site: Some("Lax".to_string()),
            }],
        };

        let cookie_store = CookieStoreMutex::new(CookieStore::default());
        load_cookie_jar_into_store(&cookie_store, &jar).unwrap();

        let out = cookie_jar_from_store(&cookie_store).unwrap();
        assert_eq!(out.cookies.len(), 1);
        assert_eq!(out.cookies[0].name, "a");
        assert_eq!(out.cookies[0].value, "b");
        assert_eq!(out.cookies[0].domain, "forge.example.com");
        assert!(!out.cookies[0].host_only);
        assert_eq!(out.cookies[0].path, "/");
        assert!(out.cookies[0].secure);
        assert!(out.cookies[0].http_only);
        assert_eq!(out.cookies[0].same_site.as_deref(), Some("Lax"));
    }

    #[test]
    fn host_only_cookie_records_roundtrip_into_cookie_store() {
        let jar = store::CookieJar {
            saved_utc: Some("2020-01-01T00:00:00Z".to_string()),
            cookies: vec![store::CookieRecord {
                name: "i_like_forgejo".to_string(),
                value: "session".to_string(),
                domain: "forge.example.com".to_string(),
                host_only: true,
                path: "/".to_string(),
                expires_utc: None,
                secure: false,
                http_only: true,
                same_site: Some("Lax".to_string()),
            }],
        };

        let cookie_store = CookieStoreMutex::new(CookieStore::default());
        load_cookie_jar_into_store(&cookie_store, &jar).unwrap();

        let out = cookie_jar_from_store(&cookie_store).unwrap();
        assert_eq!(out.cookies.len(), 1);
        assert_eq!(out.cookies[0].domain, "forge.example.com");
        assert!(out.cookies[0].host_only);
    }
}
