use std::path::PathBuf;
use std::str::FromStr;

use eyre::{eyre, Context};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoName {
    owner: String,
    name: String,
}

impl RepoName {
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn as_owner_slash_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct RepoArg {
    pub host: Option<String>,
    pub owner: String,
    pub name: String,
}

impl std::fmt::Display for RepoArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{host}/{}/{}", self.owner, self.name),
            None => write!(f, "{}/{}", self.owner, self.name),
        }
    }
}

impl FromStr for RepoArg {
    type Err = RepoArgError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, name) = s.rsplit_once('/').ok_or(RepoArgError::NoOwner)?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        let (host, owner) = match head.rsplit_once('/') {
            Some((host, owner)) => (Some(host), owner),
            None => (None, head),
        };

        Ok(Self {
            host: host.map(ToOwned::to_owned),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoArgError {
    NoOwner,
}

impl std::error::Error for RepoArgError {}

impl std::fmt::Display for RepoArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoArgError::NoOwner => {
                write!(f, "repo name should be in the format [HOST/]OWNER/NAME")
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub base_url: String,
    pub repo: Option<String>,
    pub owner: Option<String>,
    pub name: Option<String>,
    pub unix_socket: Option<PathBuf>,
}

pub fn parse_unix_socket_url(url: &str) -> Option<(PathBuf, String)> {
    if !url.starts_with("http+unix://") {
        return None;
    }

    let without_scheme = url.trim_start_matches("http+unix://");

    // Percent-decode the path
    let decoded = urlencoding::decode(without_scheme).ok()?;

    // Validate the decoded path is non-empty
    if decoded.trim().is_empty() {
        return None;
    }

    let socket_path = PathBuf::from(decoded.as_ref());

    let base_url = "http://localhost".to_string();
    Some((socket_path, base_url))
}

pub fn normalize_base_url(host_or_url: &str) -> eyre::Result<String> {
    let trimmed = host_or_url.trim();
    if trimmed.is_empty() {
        return Err(eyre!("host is required"));
    }

    if trimmed.starts_with("http+unix://") {
        #[cfg(not(unix))]
        {
            return Err(eyre!(
                "Unix socket URLs (http+unix://) are only supported on Unix platforms"
            ));
        }

        #[cfg(unix)]
        {
            // Validate that the URL contains a non-empty socket path
            // This prevents malformed URLs like "http+unix://" from passing through
            // and later falling back to TCP localhost with confusing errors
            if parse_unix_socket_url(trimmed).is_none() {
                return Err(eyre!(
                    "Invalid Unix socket URL: '{}'. Expected format: http+unix:///path/to/socket.sock",
                    trimmed
                ));
            }
            // Preserve the original http+unix:// URL for storage key uniqueness
            // (different sockets should have different credential stores)
            return Ok(trimmed.to_string());
        }
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let uri = Url::parse(trimmed).wrap_err("invalid base url")?;
        return normalized_http_base_url(&uri);
    }

    let no_frag = trimmed.split('#').next().unwrap_or(trimmed);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let host_or_base = no_query.trim_end_matches('/');
    if host_or_base.is_empty() {
        return Err(eyre!("host is required"));
    }

    let uri = Url::parse(&format!("https://{host_or_base}")).wrap_err("invalid base url")?;
    normalized_http_base_url(&uri)
}

fn normalized_http_base_url(uri: &Url) -> eyre::Result<String> {
    let host = uri.host_str().ok_or_else(|| eyre!("url is missing host"))?;
    let port = uri.port();
    let port_part = port.map(|p| format!(":{p}")).unwrap_or_default();
    let mut base = format!("{}://{}{}", uri.scheme(), host, port_part);

    let path = uri.path().trim_end_matches('/');
    if !path.is_empty() && path != "/" {
        base.push_str(path);
    }

    Ok(base)
}

pub fn normalize_host_key(host_or_url: &str) -> eyre::Result<String> {
    let trimmed = host_or_url.trim();
    if trimmed.is_empty() {
        return Err(eyre!("host is required"));
    }

    if trimmed.starts_with("http+unix://") {
        return Ok(trimmed.to_string());
    }

    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("ssh://")
    {
        let uri = Url::parse(trimmed).wrap_err("invalid host url")?;
        let host = uri.host_str().ok_or_else(|| eyre!("url is missing host"))?;
        if let Some(port) = uri.port() {
            return Ok(format!("{host}:{port}"));
        }
        return Ok(host.to_string());
    }

    let no_frag = trimmed.split('#').next().unwrap_or(trimmed);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let host_part = no_query.split('/').next().unwrap_or(no_query);
    Ok(host_part.trim().to_string())
}

fn fallback_host_from_env() -> Option<Url> {
    let envvar = std::env::var_os("FJ_FALLBACK_HOST")?;
    let raw = envvar.to_str()?;
    let out = raw
        .parse::<Url>()
        .ok()
        .or_else(|| Url::parse(&format!("https://{raw}")).ok());
    if out.is_none() {
        println!("warn: `FJ_FALLBACK_HOST` is not set to a valid url");
    }
    out
}

fn ssh_url_parse(s: &str) -> Result<Url, url::ParseError> {
    Url::parse(s).or_else(|_| {
        let mut new_s = String::new();
        new_s.push_str("ssh://");

        let auth_end = s.find('@').unwrap_or(0);
        new_s.push_str(&s[..auth_end]);
        new_s.push_str(&s[auth_end..].replacen(':', "/", 1));
        Url::parse(&new_s)
    })
}

fn remote_url_to_host_and_repo(url_s: &str) -> eyre::Result<Option<(String, RepoName)>> {
    let url = ssh_url_parse(url_s).wrap_err("unable to parse remote url")?;
    let host = url
        .host_str()
        .ok_or_else(|| eyre!("remote url missing host"))?;
    let port_part = url.port().map(|p| format!(":{p}")).unwrap_or_default();

    let mut segments = url
        .path_segments()
        .ok_or_else(|| eyre!("remote url cannot be a base"))?
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    if segments.len() < 2 {
        return Ok(None);
    }

    let name = segments.pop().unwrap();
    let owner = segments.pop().unwrap();
    let name = name.strip_suffix(".git").unwrap_or(name);

    let mut base = if matches!(url.scheme(), "http" | "https") {
        format!("{}://{}{}", url.scheme(), host, port_part)
    } else {
        format!("{host}{port_part}")
    };
    if !segments.is_empty() {
        base.push('/');
        base.push_str(&segments.join("/"));
    }

    Ok(Some((
        base,
        RepoName {
            owner: owner.to_string(),
            name: name.to_string(),
        },
    )))
}

fn select_remote_name(
    repo: &git2::Repository,
    preferred: Option<&str>,
    host_hint: Option<&str>,
) -> eyre::Result<Option<String>> {
    let remotes = repo.remotes().wrap_err("failed to list git remotes")?;
    let remote_names: Vec<&str> = remotes.iter().flatten().collect();

    if let Some(name) = preferred {
        return Ok(Some(name.to_string()));
    }

    if remote_names.len() == 1 {
        return Ok(Some(remote_names[0].to_string()));
    }

    if let Ok(head) = repo.head() {
        if let Some(branch_name) = head.name() {
            if let Ok(remote_name) = repo.branch_upstream_remote(branch_name) {
                if let Some(remote_name) = remote_name.as_str() {
                    return Ok(Some(remote_name.to_string()));
                }
            }
        }
    }

    if let Some(host_hint) = host_hint {
        if let Ok(hint_key) = normalize_host_key(host_hint) {
            for remote_name in &remote_names {
                let remote = repo.find_remote(remote_name)?;
                let Some(url_s) = remote.url() else {
                    continue;
                };

                if let Ok(parsed) = remote_url_to_host_and_repo(url_s) {
                    if let Some((host, _)) = parsed {
                        if crate::target::normalize_host_key(&host)
                            .is_ok_and(|remote_key| remote_key == hint_key)
                        {
                            return Ok(Some((*remote_name).to_string()));
                        }
                    }
                }
            }
        }
    }

    if remote_names.iter().any(|n| *n == "origin") {
        return Ok(Some("origin".to_string()));
    }

    Ok(remote_names.first().map(|s| (*s).to_string()))
}

fn infer_from_git(
    remote: Option<&str>,
    host_hint: Option<&str>,
) -> eyre::Result<Option<(String, String)>> {
    let repo = match git2::Repository::discover(".") {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };

    let remote_name = match select_remote_name(&repo, remote, host_hint)? {
        Some(n) => n,
        None => return Ok(None),
    };

    let remote_ref = repo
        .find_remote(&remote_name)
        .wrap_err_with(|| format!("unable to find git remote '{remote_name}'"))?;
    let url_s = remote_ref
        .url()
        .ok_or_else(|| eyre!("git remote '{remote_name}' has no url"))?;

    let Some((remote_host, remote_repo)) = remote_url_to_host_and_repo(url_s)? else {
        return Ok(None);
    };

    let base_url = normalize_base_url(&remote_host)?;
    Ok(Some((base_url, remote_repo.as_owner_slash_name())))
}

pub fn resolve_target(
    host: Option<&str>,
    repo: Option<&RepoArg>,
    remote: Option<&str>,
) -> eyre::Result<ResolvedTarget> {
    let mut resolved_repo: Option<RepoName> = repo.map(|r| RepoName {
        owner: r.owner.clone(),
        name: r.name.clone(),
    });

    let mut resolved_base_url: Option<String> = None;
    let mut resolved_unix_socket: Option<PathBuf> = None;
    let mut raw_host: Option<String> = None;

    if let Some(repo) = repo {
        if let Some(repo_host) = repo.host.as_deref() {
            raw_host = Some(repo_host.to_string());
            // Normalize first to ensure platform validation happens
            let normalized = normalize_base_url(repo_host)?;
            resolved_base_url = Some(normalized.clone());
            // Then extract socket from the normalized URL
            if let Some((socket, _base)) = parse_unix_socket_url(&normalized) {
                resolved_unix_socket = Some(socket);
            }
        }
    }

    if resolved_base_url.is_none() {
        if let Some(host) = host {
            raw_host = Some(host.to_string());
            // Normalize first to ensure platform validation happens
            let normalized = normalize_base_url(host)?;
            resolved_base_url = Some(normalized.clone());
            // Then extract socket from the normalized URL
            if let Some((socket, _base)) = parse_unix_socket_url(&normalized) {
                resolved_unix_socket = Some(socket);
            }
        }
    }

    if resolved_base_url.is_none() || resolved_repo.is_none() {
        if let Some((base, repo_name)) = infer_from_git(remote, raw_host.as_deref())? {
            resolved_base_url.get_or_insert(base);
            if resolved_repo.is_none() {
                resolved_repo = Some(
                    RepoArg::from_str(&repo_name)
                        .map_err(|e| eyre!(e))
                        .wrap_err("failed to parse inferred repo")?
                        .into(),
                );
            }
        }
    }

    if resolved_base_url.is_none() {
        if let Some(url) = fallback_host_from_env() {
            let url_str = url.as_str();
            // Normalize first to ensure platform validation happens
            let normalized = normalize_base_url(url_str)?;
            resolved_base_url = Some(normalized.clone());
            // Then extract socket from the normalized URL
            if let Some((socket, _base)) = parse_unix_socket_url(&normalized) {
                resolved_unix_socket = Some(socket);
            }
        }
    }

    let base_url = resolved_base_url.ok_or_else(|| {
        eyre!(
            "unable to resolve Forgejo host. Pass --host, set FJ_FALLBACK_HOST, or run inside a git repo with a Forgejo remote."
        )
    })?;

    Ok(ResolvedTarget {
        repo: resolved_repo.as_ref().map(|r| r.as_owner_slash_name()),
        owner: resolved_repo.as_ref().map(|r| r.owner.clone()),
        name: resolved_repo.as_ref().map(|r| r.name.clone()),
        base_url,
        unix_socket: resolved_unix_socket,
    })
}

impl From<RepoArg> for RepoName {
    fn from(value: RepoArg) -> Self {
        Self {
            owner: value.owner,
            name: value.name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_accepts_host() {
        assert_eq!(
            normalize_base_url("forge.example.com").unwrap(),
            "https://forge.example.com"
        );
    }

    #[test]
    fn normalize_base_url_preserves_path_prefix() {
        assert_eq!(
            normalize_base_url("https://forge.example.com:3000/gitea/").unwrap(),
            "https://forge.example.com:3000/gitea"
        );
    }

    #[test]
    fn normalize_base_url_accepts_bare_host_with_path() {
        assert_eq!(
            normalize_base_url("forge.example.com:3000/gitea").unwrap(),
            "https://forge.example.com:3000/gitea"
        );
    }

    #[test]
    fn normalize_host_key_strips_path() {
        assert_eq!(
            normalize_host_key("forge.example.com:3000/some/path").unwrap(),
            "forge.example.com:3000"
        );
    }

    #[test]
    fn repo_arg_parses_owner_name() {
        let r = RepoArg::from_str("alice/widgets").unwrap();
        assert_eq!(r.host, None);
        assert_eq!(r.owner, "alice");
        assert_eq!(r.name, "widgets");
    }

    #[test]
    fn repo_arg_parses_host_owner_name() {
        let r = RepoArg::from_str("forge.example.com/alice/widgets.git").unwrap();
        assert_eq!(r.host.as_deref(), Some("forge.example.com"));
        assert_eq!(r.owner, "alice");
        assert_eq!(r.name, "widgets");
    }

    #[test]
    fn remote_url_parses_https() {
        let (host, repo) =
            remote_url_to_host_and_repo("https://forge.example.com/alice/widgets.git")
                .unwrap()
                .unwrap();
        assert_eq!(host, "https://forge.example.com");
        assert_eq!(repo.as_owner_slash_name(), "alice/widgets");
    }

    #[test]
    fn remote_url_parses_https_with_base_path() {
        let (host, repo) =
            remote_url_to_host_and_repo("https://forge.example.com/gitea/alice/widgets.git")
                .unwrap()
                .unwrap();
        assert_eq!(host, "https://forge.example.com/gitea");
        assert_eq!(repo.as_owner_slash_name(), "alice/widgets");
    }

    #[test]
    fn remote_url_parses_ssh_scp_style() {
        let (host, repo) = remote_url_to_host_and_repo("git@forge.example.com:alice/widgets.git")
            .unwrap()
            .unwrap();
        assert_eq!(host, "forge.example.com");
        assert_eq!(repo.as_owner_slash_name(), "alice/widgets");
    }

    #[test]
    fn parse_unix_socket_url_works() {
        let result = parse_unix_socket_url("http+unix:///run/forgejo/http.sock");
        assert!(result.is_some());
        let (socket, base) = result.unwrap();
        assert_eq!(socket, PathBuf::from("/run/forgejo/http.sock"));
        assert_eq!(base, "http://localhost");
    }

    #[test]
    fn parse_unix_socket_url_with_dynamic_path() {
        // Test with a dynamic path to verify the parser works correctly
        let test_path = PathBuf::from("/tmp/test/socket.sock");
        let socket_url = format!("http+unix://{}", test_path.display());

        let result = parse_unix_socket_url(&socket_url);
        assert!(result.is_some());

        let (parsed_socket, parsed_base) = result.unwrap();
        assert_eq!(parsed_socket, test_path);
        assert_eq!(parsed_base, "http://localhost");
    }

    #[test]
    fn parse_unix_socket_url_with_percent_encoding() {
        // Test percent-encoded paths
        let result = parse_unix_socket_url("http+unix:///run/my%20socket/http.sock");
        assert!(result.is_some());
        let (socket, base) = result.unwrap();
        assert_eq!(socket, PathBuf::from("/run/my socket/http.sock"));
        assert_eq!(base, "http://localhost");
    }

    #[test]
    #[cfg(unix)]
    fn normalize_base_url_handles_unix_socket() {
        assert_eq!(
            normalize_base_url("http+unix:///run/forgejo/http.sock").unwrap(),
            "http+unix:///run/forgejo/http.sock"
        );
    }

    #[test]
    #[cfg(not(unix))]
    fn normalize_base_url_rejects_unix_socket_on_non_unix() {
        assert!(normalize_base_url("http+unix:///run/forgejo/http.sock").is_err());
    }

    #[test]
    fn normalize_host_key_preserves_unix_socket() {
        assert_eq!(
            normalize_host_key("http+unix:///run/forgejo/http.sock").unwrap(),
            "http+unix:///run/forgejo/http.sock"
        );
    }

    #[test]
    fn parse_unix_socket_url_rejects_empty_path() {
        assert_eq!(parse_unix_socket_url("http+unix://"), None);
        assert_eq!(parse_unix_socket_url("http+unix://   "), None);
    }

    #[test]
    #[cfg(unix)]
    fn normalize_base_url_rejects_malformed_unix_socket_url() {
        // Empty path should fail
        assert!(normalize_base_url("http+unix://").is_err());
        // Whitespace-only path should fail
        assert!(normalize_base_url("http+unix://   ").is_err());
    }
}
