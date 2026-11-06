//! The single table of server targets exercised by the Docker E2E suite.
//!
//! Adding a server version is one row in [`TARGETS`], one `e2e_target!` line in
//! `tests/e2e_docker.rs`, and one entry in the CI matrix.

/// Server family. Decides the server CLI binary and how the runner registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Forgejo,
    Gitea,
}

impl Family {
    /// Binary inside the server image used for `admin user create` etc.
    pub fn server_bin(self) -> &'static str {
        match self {
            Family::Forgejo => "forgejo",
            Family::Gitea => "gitea",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Family::Forgejo => "Forgejo",
            Family::Gitea => "Gitea",
        }
    }
}

/// Per-target capability flags.
///
/// A flag is `false` only when the server genuinely lacks a feature; the
/// scenario that needs it then logs the reason (see [`Caps::explain`]) and
/// the check shows up as SKIPPED in the summary instead of silently passing.
/// No target currently needs a gate: every feature works on all six.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Listing the runner job queue (`fj-ex actions runners jobs`). Forgejo
    /// serves `/actions/runners/jobs`; on Gitea fj-ex falls back to
    /// `/actions/jobs`. Turn off for a server that has neither.
    pub runner_jobs_api: bool,
}

impl Caps {
    pub const ALL: Caps = Caps {
        runner_jobs_api: true,
    };

    /// Human readable reason for a disabled capability.
    pub fn explain(name: &str) -> &'static str {
        match name {
            "runner_jobs_api" => {
                "server has neither /actions/runners/jobs (Forgejo) nor /actions/jobs (Gitea)"
            }
            _ => "capability disabled for this target",
        }
    }
}

/// One server version under test.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// Stable identifier used in test names, `FJ_EX_E2E_TARGET` and container names.
    pub id: &'static str,
    pub family: Family,
    pub version: &'static str,
    pub image: &'static str,
    pub runner_image: &'static str,
    /// Directory holding workflows in the seeded repositories (`.forgejo` or `.gitea`).
    pub workflow_dir: &'static str,
    pub caps: Caps,
}

impl Target {
    pub fn label(&self) -> String {
        format!("{} {}", self.family.name(), self.version)
    }
}

const fn t(
    id: &'static str,
    family: Family,
    version: &'static str,
    image: &'static str,
    runner_image: &'static str,
    workflow_dir: &'static str,
    caps: Caps,
) -> Target {
    Target {
        id,
        family,
        version,
        image,
        runner_image,
        workflow_dir,
        caps,
    }
}

/// Forgejo's own runner (registration-token flow via `forgejo-runner register`).
pub const FORGEJO_RUNNER_IMAGE: &str = "data.forgejo.org/forgejo/runner:13.2.1";
/// Gitea's runner (formerly act_runner); its entrypoint registers from env vars.
pub const GITEA_RUNNER_IMAGE: &str = "gitea/runner:5.4.1";

use Family::{Forgejo, Gitea};

#[rustfmt::skip]
pub const TARGETS: &[Target] = &[
    //  id            family   version    server image                              runner image          workflow dir  capabilities
    t("forgejo-15",  Forgejo, "15.0.11", "codeberg.org/forgejo/forgejo:15.0.11",   FORGEJO_RUNNER_IMAGE, ".forgejo",   Caps::ALL),
    t("forgejo-16",  Forgejo, "16.0.7",  "codeberg.org/forgejo/forgejo:16.0.7",    FORGEJO_RUNNER_IMAGE, ".forgejo",   Caps::ALL),
    t("forgejo-17",  Forgejo, "17.0.1",  "codeberg.org/forgejo/forgejo:17.0.1",    FORGEJO_RUNNER_IMAGE, ".forgejo",   Caps::ALL),
    t("gitea-1-25",  Gitea,   "1.25.5",  "docker.gitea.com/gitea:1.25.5",          GITEA_RUNNER_IMAGE,   ".gitea",     Caps::ALL),
    t("gitea-1-26",  Gitea,   "1.26.4",  "docker.gitea.com/gitea:1.26.4",          GITEA_RUNNER_IMAGE,   ".gitea",     Caps::ALL),
    t("gitea-1-27",  Gitea,   "1.27.3",  "docker.gitea.com/gitea:1.27.3",          GITEA_RUNNER_IMAGE,   ".gitea",     Caps::ALL),
];

/// Image used for job containers (`runs-on: ubuntu-latest`). It only needs
/// bash + curl because the fixture workflows use no remote actions.
pub const JOB_IMAGE: &str = "buildpack-deps:bookworm-curl";
pub const RUNNER_LABEL: &str = "ubuntu-latest";

pub fn find(id: &str) -> Option<&'static Target> {
    TARGETS.iter().find(|t| t.id == id)
}

/// Targets selected by `FJ_EX_E2E_TARGET` (comma separated ids, or `all`).
/// `None` means "no filter".
pub fn selection_from_env() -> Option<Vec<String>> {
    let raw = std::env::var("FJ_EX_E2E_TARGET").ok()?;
    let ids: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() || ids.iter().any(|s| s == "all") {
        return None;
    }
    Some(ids)
}

pub fn is_selected(id: &str) -> bool {
    selection_from_env().is_none_or(|ids| ids.iter().any(|s| s == id))
}
