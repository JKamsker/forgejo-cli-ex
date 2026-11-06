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

/// Whether the server's REST API (used by `fj-ex actions` with only an API
/// token) has the endpoint for an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Yes,
    /// Missing here; added in this release (fj-ex must say so).
    Since(&'static str),
    /// No such endpoint in any release of this family.
    No,
}

/// Per-target capability flags.
///
/// A flag is off only when the server genuinely lacks a feature. Scenarios
/// then log the gate (see [`Caps::explain`]) and, for token-only REST calls,
/// assert that fj-ex refuses with the expected "added in ..." / "no
/// endpoint" message instead of silently passing.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Listing the runner job queue (`fj-ex actions runners jobs`). Forgejo
    /// serves `/actions/runners/jobs`; on Gitea fj-ex falls back to
    /// `/actions/jobs`. Turn off for a server that has neither.
    pub runner_jobs_api: bool,
    /// REST: list the jobs of a run (`actions jobs`, `wait`).
    pub api_jobs: Api,
    /// REST: download job logs.
    pub api_logs: Api,
    /// REST: cancel a run.
    pub api_cancel: Api,
    /// REST: rerun failed jobs.
    pub api_rerun_failed: Api,
    /// REST: list workflows.
    pub api_workflows: Api,
    /// Name of the web session cookie (for `auth login --session-cookie-stdin`).
    pub session_cookie: &'static str,
}

impl Caps {
    /// Human readable reason for a disabled capability.
    pub fn explain(name: &str) -> &'static str {
        match name {
            "runner_jobs_api" => {
                "server has neither /actions/runners/jobs (Forgejo) nor /actions/jobs (Gitea)"
            }
            "api_jobs" => "the REST API cannot list a run's jobs",
            "api_logs" => "the REST API cannot download job logs",
            "api_cancel" => "the REST API cannot cancel runs",
            "api_rerun_failed" => "the REST API cannot rerun failed jobs",
            "api_workflows" => "the REST API cannot list workflows",
            _ => "capability disabled for this target",
        }
    }
}

const FORGEJO_15_CAPS: Caps = Caps {
    runner_jobs_api: true,
    api_jobs: Api::Since("Forgejo 16"),
    api_logs: Api::Since("Forgejo 16"),
    api_cancel: Api::Since("Forgejo 16"),
    api_rerun_failed: Api::Since("Forgejo 17"),
    api_workflows: Api::No,
    session_cookie: "session",
};
const FORGEJO_16_CAPS: Caps = Caps {
    api_jobs: Api::Yes,
    api_logs: Api::Yes,
    api_cancel: Api::Yes,
    ..FORGEJO_15_CAPS
};
const FORGEJO_17_CAPS: Caps = Caps {
    api_rerun_failed: Api::Yes,
    ..FORGEJO_16_CAPS
};
const GITEA_1_25_CAPS: Caps = Caps {
    runner_jobs_api: true,
    api_jobs: Api::Yes,
    api_logs: Api::Yes,
    api_cancel: Api::No,
    api_rerun_failed: Api::Since("Gitea 1.26"),
    api_workflows: Api::Yes,
    session_cookie: "i_like_gitea",
};
const GITEA_1_26_CAPS: Caps = Caps {
    api_rerun_failed: Api::Yes,
    ..GITEA_1_25_CAPS
};

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
    t("forgejo-15",  Forgejo, "15.0.11", "codeberg.org/forgejo/forgejo:15.0.11",   FORGEJO_RUNNER_IMAGE, ".forgejo",   FORGEJO_15_CAPS),
    t("forgejo-16",  Forgejo, "16.0.7",  "codeberg.org/forgejo/forgejo:16.0.7",    FORGEJO_RUNNER_IMAGE, ".forgejo",   FORGEJO_16_CAPS),
    t("forgejo-17",  Forgejo, "17.0.1",  "codeberg.org/forgejo/forgejo:17.0.1",    FORGEJO_RUNNER_IMAGE, ".forgejo",   FORGEJO_17_CAPS),
    t("gitea-1-25",  Gitea,   "1.25.5",  "docker.gitea.com/gitea:1.25.5",          GITEA_RUNNER_IMAGE,   ".gitea",     GITEA_1_25_CAPS),
    t("gitea-1-26",  Gitea,   "1.26.4",  "docker.gitea.com/gitea:1.26.4",          GITEA_RUNNER_IMAGE,   ".gitea",     GITEA_1_26_CAPS),
    t("gitea-1-27",  Gitea,   "1.27.3",  "docker.gitea.com/gitea:1.27.3",          GITEA_RUNNER_IMAGE,   ".gitea",     GITEA_1_26_CAPS),
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
