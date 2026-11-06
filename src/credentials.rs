//! Resolution of API credentials (tokens and basic auth) shared by all
//! REST-API backed commands.
//!
//! Token precedence (first match wins):
//! 1. a token stored by `fj-ex auth login --token-stdin` / `--token` for the base URL,
//! 2. the environment variables `FJ_EX_TOKEN`, `FORGEJO_TOKEN`, `GITEA_TOKEN` (in that order),
//! 3. the token stored by the official `fj` CLI in its `keys.json`.
//!
//! Commands that historically used the `fj-ex` password login for REST calls
//! (`actions secrets/variables set`, `actions trigger`, `pulls`, ...) fall back
//! to HTTP basic auth with the stored password when no token is found.

use std::fmt;

use eyre::{eyre, Context};
use reqwest::header::HeaderValue;

/// Environment variables consulted for an API token, in priority order.
pub const TOKEN_ENV_VARS: [&str; 3] = ["FJ_EX_TOKEN", "FORGEJO_TOKEN", "GITEA_TOKEN"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenSource {
    /// Stored in ui-creds.json by `fj-ex auth login --token[-stdin]`.
    Stored,
    /// Read from an environment variable.
    Env(&'static str),
    /// Read from the official `fj` CLI's keys.json.
    FjKeys,
}

impl TokenSource {
    /// Stable machine-readable identifier (used in JSON output).
    pub fn id(&self) -> String {
        match self {
            Self::Stored => "stored".to_string(),
            Self::Env(name) => format!("env:{name}"),
            Self::FjKeys => "fj-keys".to_string(),
        }
    }
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stored => write!(f, "API token stored by `fj-ex auth login`"),
            Self::Env(name) => write!(f, "API token from ${name}"),
            Self::FjKeys => write!(f, "API token from fj keys.json"),
        }
    }
}

/// An API token together with where it came from. `Debug` never prints the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedToken {
    pub token: String,
    pub source: TokenSource,
}

impl fmt::Debug for ResolvedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedToken")
            .field("token", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// Apply the documented token precedence. Pure so it can be unit tested.
pub fn select_token(
    stored: Option<String>,
    env: impl Fn(&str) -> Option<String>,
    fj_keys: impl FnOnce() -> eyre::Result<Option<String>>,
) -> eyre::Result<Option<ResolvedToken>> {
    if let Some(token) = clean(stored) {
        return Ok(Some(ResolvedToken {
            token,
            source: TokenSource::Stored,
        }));
    }

    for name in TOKEN_ENV_VARS {
        if let Some(token) = clean(env(name)) {
            return Ok(Some(ResolvedToken {
                token,
                source: TokenSource::Env(name),
            }));
        }
    }

    Ok(clean(fj_keys()?).map(|token| ResolvedToken {
        token,
        source: TokenSource::FjKeys,
    }))
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve an API token for `base_url` from all supported sources.
pub async fn resolve_api_token(base_url: &str) -> eyre::Result<Option<ResolvedToken>> {
    let stored = crate::store::get_stored_api_token(base_url).await?;
    select_token(
        stored,
        |name| std::env::var(name).ok(),
        || crate::store::get_fj_api_token_for_base_url(base_url),
    )
}

/// Credentials for a REST API request.
#[derive(Clone)]
pub enum ApiAuth {
    Token(ResolvedToken),
    Basic(crate::store::UiCreds),
}

impl fmt::Debug for ApiAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token(token) => f.debug_tuple("Token").field(token).finish(),
            Self::Basic(creds) => f
                .debug_struct("Basic")
                .field("username", &creds.username)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

impl ApiAuth {
    /// Human-readable description of the credential source (never the secret).
    pub fn source_label(&self) -> String {
        match self {
            Self::Token(token) => token.source.to_string(),
            Self::Basic(_) => "stored UI basic credentials".to_string(),
        }
    }

    /// `Authorization` header value for this credential.
    pub fn header_value(&self) -> eyre::Result<HeaderValue> {
        let raw = match self {
            Self::Token(token) => format!("token {}", token.token),
            Self::Basic(creds) => format!(
                "Basic {}",
                base64_encode(format!("{}:{}", creds.username, creds.password).as_bytes())
            ),
        };
        let mut value = HeaderValue::from_str(&raw)
            .map_err(|_| eyre!("credential contains characters that are invalid in an HTTP header"))
            .wrap_err_with(|| format!("invalid {}", self.source_label()))?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// Attach this credential to a request.
    pub fn apply(&self, request: reqwest::RequestBuilder) -> eyre::Result<reqwest::RequestBuilder> {
        Ok(request.header(reqwest::header::AUTHORIZATION, self.header_value()?))
    }
}

/// Resolve credentials for REST API commands: a token from any source, else
/// the stored password login as basic auth.
pub async fn resolve_api_auth(base_url: &str) -> eyre::Result<Option<ApiAuth>> {
    let token = resolve_api_token(base_url).await;
    if let Ok(Some(token)) = token {
        return Ok(Some(ApiAuth::Token(token)));
    }
    let basic = crate::store::get_ui_creds(base_url).await?;
    match (token, basic) {
        (Err(err), Some(creds)) => {
            // An unreadable token source (e.g. a corrupt fj keys.json) must not
            // break commands that work with the stored password.
            warn_unreadable_token_source(&err);
            Ok(Some(ApiAuth::Basic(creds)))
        }
        (Err(err), None) => Err(err),
        (Ok(_), basic) => Ok(basic.map(ApiAuth::Basic)),
    }
}

/// Resolve a token, but treat an unreadable token source as "no token" (with a
/// warning). For callers that have another credential to fall back on.
pub async fn resolve_api_token_lenient(base_url: &str) -> Option<ResolvedToken> {
    match resolve_api_token(base_url).await {
        Ok(token) => token,
        Err(err) => {
            warn_unreadable_token_source(&err);
            None
        }
    }
}

fn warn_unreadable_token_source(err: &eyre::Report) {
    eprintln!(
        "warn: ignoring an unreadable API token source: {}",
        err.to_string().lines().next().unwrap_or_default()
    );
}

/// Error for commands that need a REST API credential (token or password).
pub fn missing_api_auth_error(base_url: &str) -> eyre::Report {
    eyre!(
        "No API token or stored password login for '{base_url}'.\n\
         Provide one of:\n  \
         fj-ex auth login --host {base_url} --token-stdin   # API token (recommended for automation and SSO accounts)\n  \
         export FJ_EX_TOKEN=...                             # or FORGEJO_TOKEN / GITEA_TOKEN\n  \
         fj-ex auth login --host {base_url}                 # username + password"
    )
}

/// Error for commands that require an API token (e.g. runner administration).
pub fn missing_api_token_error(base_url: &str) -> eyre::Report {
    let token_key = crate::target::normalize_base_url(base_url)
        .map(|normalized| {
            if crate::target::normalized_base_url_has_path(&normalized) {
                normalized
            } else {
                crate::target::normalize_host_key(&normalized).unwrap_or(normalized)
            }
        })
        .unwrap_or_else(|_| base_url.into());
    let path = crate::store::keys_store_paths()
        .map(|p| p.path.display().to_string())
        .unwrap_or_else(|_| "%APPDATA%/Cyborus/forgejo-cli/data/keys.json".to_string());

    eyre!(
        "No Forgejo API token found for host '{token_key}'. Provide a token with one of:\n  \
         fj-ex auth login --host {token_key} --token-stdin\n  \
         export FJ_EX_TOKEN=...   # or FORGEJO_TOKEN / GITEA_TOKEN\n\
         or authenticate via `fj`:\n  \
         fj --host {token_key} auth login\n  \
         fj --host {token_key} auth add-key <USER>  # reads token from stdin if omitted\n\
         Token store:\n  \
         %APPDATA%/Cyborus/forgejo-cli/data/keys.json (Windows)\n  \
         ~/.local/share/Cyborus/forgejo-cli/data/keys.json (Linux)\n\
         Reading: {path}"
    )
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn stored_token_wins_over_env_and_fj_keys() {
        let token = select_token(
            Some("stored".into()),
            env_of(&[("FJ_EX_TOKEN", "env"), ("FORGEJO_TOKEN", "fg")]),
            || panic!("fj keys must not be read when a stored token exists"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(token.token, "stored");
        assert_eq!(token.source, TokenSource::Stored);
    }

    #[test]
    fn env_tokens_follow_documented_order() {
        let pick = |pairs: &[(&str, &str)]| {
            select_token(None, env_of(pairs), || Ok(Some("fj".into())))
                .unwrap()
                .unwrap()
        };

        let t = pick(&[
            ("GITEA_TOKEN", "gt"),
            ("FORGEJO_TOKEN", "fg"),
            ("FJ_EX_TOKEN", "fx"),
        ]);
        assert_eq!(
            (t.token.as_str(), t.source),
            ("fx", TokenSource::Env("FJ_EX_TOKEN"))
        );

        let t = pick(&[("GITEA_TOKEN", "gt"), ("FORGEJO_TOKEN", "fg")]);
        assert_eq!(t.source, TokenSource::Env("FORGEJO_TOKEN"));

        let t = pick(&[("GITEA_TOKEN", "gt")]);
        assert_eq!(t.source, TokenSource::Env("GITEA_TOKEN"));

        let t = pick(&[]);
        assert_eq!((t.token.as_str(), t.source), ("fj", TokenSource::FjKeys));
    }

    #[test]
    fn blank_values_are_ignored() {
        let token = select_token(
            Some("   ".into()),
            env_of(&[("FJ_EX_TOKEN", ""), ("FORGEJO_TOKEN", " tok \n")]),
            || Ok(None),
        )
        .unwrap()
        .unwrap();
        assert_eq!(token.token, "tok");
        assert_eq!(token.source, TokenSource::Env("FORGEJO_TOKEN"));

        assert!(select_token(None, env_of(&[]), || Ok(Some(" ".into())))
            .unwrap()
            .is_none());
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let token = ResolvedToken {
            token: "super-secret".into(),
            source: TokenSource::Env("FORGEJO_TOKEN"),
        };
        assert!(!format!("{token:?}").contains("super-secret"));
        let basic = ApiAuth::Basic(crate::store::UiCreds {
            base_url: "https://x".into(),
            username: "alice".into(),
            password: "hunter2".into(),
            updated_utc: None,
        });
        assert!(!format!("{basic:?}").contains("hunter2"));
        assert!(!basic.source_label().contains("hunter2"));
    }

    #[test]
    fn token_source_ids_are_stable() {
        assert_eq!(TokenSource::Stored.id(), "stored");
        assert_eq!(TokenSource::Env("GITEA_TOKEN").id(), "env:GITEA_TOKEN");
        assert_eq!(TokenSource::FjKeys.id(), "fj-keys");
    }

    #[test]
    fn basic_header_uses_standard_base64() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"alice:secret"), "YWxpY2U6c2VjcmV0");
    }
}
