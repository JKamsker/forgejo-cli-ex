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
}

pub fn normalize_base_url(host_or_url: &str) -> eyre::Result<String> {
    let trimmed = host_or_url.trim();
    if trimmed.is_empty() {
        return Err(eyre!("host is required"));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let uri = Url::parse(trimmed).wrap_err("invalid base url")?;
        let host = uri.host_str().ok_or_else(|| eyre!("url is missing host"))?;
        let port = uri.port();
        let port_part = port.map(|p| format!(":{p}")).unwrap_or_default();
        return Ok(format!("{}://{}{}", uri.scheme(), host, port_part));
    }

    let no_frag = trimmed.split('#').next().unwrap_or(trimmed);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let host_part = no_query
        .split('/')
        .next()
        .unwrap_or(no_query)
        .trim_end_matches('/');
    if host_part.is_empty() {
        return Err(eyre!("host is required"));
    }
    Ok(format!("https://{host_part}"))
}

pub fn normalize_host_key(host_or_url: &str) -> eyre::Result<String> {
    let trimmed = host_or_url.trim();
    if trimmed.is_empty() {
        return Err(eyre!("host is required"));
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

    Ok(Some((
        host.to_string(),
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
                        if host == hint_key {
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

    if let Some(repo) = repo {
        if let Some(repo_host) = repo.host.as_deref() {
            resolved_base_url = Some(normalize_base_url(repo_host)?);
        }
    }

    if resolved_base_url.is_none() {
        if let Some(host) = host {
            resolved_base_url = Some(normalize_base_url(host)?);
        }
    }

    if resolved_base_url.is_none() || resolved_repo.is_none() {
        if let Some((base, repo_name)) = infer_from_git(remote, host)? {
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
            let base = normalize_base_url(url.as_str())?;
            resolved_base_url = Some(base);
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
    fn normalize_base_url_strips_path() {
        assert_eq!(
            normalize_base_url("https://forge.example.com:3000/some/path").unwrap(),
            "https://forge.example.com:3000"
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
        assert_eq!(host, "forge.example.com");
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
}
