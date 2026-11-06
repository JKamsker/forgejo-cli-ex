//! Backend selection for `fj-ex actions …`: the web UI (cookie session) or the
//! REST API (API token).

use eyre::{eyre, Context};
use serde_json::Value;

use crate::api_actions::ApiActions;
use crate::cli::ActionsBackendMode;
use crate::session::UiSession;
use crate::ui_actions::{JobInfo, RunRef};

pub enum ActionsClient {
    Ui(UiSession),
    Api(Box<ApiActions>),
}

pub struct RerunFailedResult {
    pub failures: Vec<String>,
    pub redirects: Vec<String>,
}

/// Which credentials are available, used to pick a backend in `auto` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoChoice {
    /// A UI login (password or imported session) is stored: use the UI.
    Ui,
    /// No UI login but an API credential: use the API.
    Api,
    /// Nothing usable.
    None,
}

pub fn auto_choice(has_ui_login: bool, has_api_token: bool) -> AutoChoice {
    match (has_ui_login, has_api_token) {
        (true, _) => AutoChoice::Ui,
        (false, true) => AutoChoice::Api,
        (false, false) => AutoChoice::None,
    }
}

/// Build the actions client for `mode`.
///
/// `auto`: use the UI when a password login or imported session is stored
/// (this keeps the cookie flow working exactly as before); otherwise use the
/// REST API with a token. If the UI session cannot be established (e.g. an
/// imported session expired) and a token is available, fall back to the API.
pub async fn connect(
    mode: ActionsBackendMode,
    target: &crate::target::ResolvedTarget,
    repo: &str,
) -> eyre::Result<ActionsClient> {
    match mode {
        ActionsBackendMode::Ui => Ok(ActionsClient::Ui(ui_session(target).await?)),
        ActionsBackendMode::Api => {
            let auth = crate::credentials::resolve_api_auth(&target.base_url)
                .await?
                .ok_or_else(|| crate::credentials::missing_api_auth_error(&target.base_url))?;
            api_client(target, repo, &auth).await
        }
        ActionsBackendMode::Auto => {
            let info = crate::store::get_store_entry(&target.base_url).await?;
            let has_ui_login = info
                .entry
                .as_ref()
                .and_then(crate::store::StoreEntry::ui_login_kind)
                .is_some();
            let token = if has_ui_login {
                // The UI flow must keep working even if a token source is unreadable.
                crate::credentials::resolve_api_token_lenient(&target.base_url).await
            } else {
                crate::credentials::resolve_api_token(&target.base_url).await?
            };

            match auto_choice(has_ui_login, token.is_some()) {
                AutoChoice::Ui => match ui_session(target).await {
                    Ok(session) => Ok(ActionsClient::Ui(session)),
                    Err(ui_err) => {
                        let Some(token) = token else {
                            return Err(ui_err);
                        };
                        eprintln!(
                            "warn: UI session unavailable; falling back to the REST API with the {}.\n      reason: {}",
                            token.source,
                            first_line(&ui_err)
                        );
                        let auth = crate::credentials::ApiAuth::Token(token);
                        api_client(target, repo, &auth).await
                    }
                },
                AutoChoice::Api => {
                    let auth = crate::credentials::ApiAuth::Token(
                        token.expect("token present for AutoChoice::Api"),
                    );
                    api_client(target, repo, &auth).await
                }
                AutoChoice::None => Err(crate::session::no_ui_login_error(
                    &target.base_url,
                    info.entry.as_ref(),
                )),
            }
        }
    }
}

fn first_line(err: &eyre::Report) -> String {
    err.to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

async fn ui_session(target: &crate::target::ResolvedTarget) -> eyre::Result<UiSession> {
    UiSession::from_store_with_socket(&target.base_url, false, target.unix_socket.as_deref()).await
}

async fn api_client(
    target: &crate::target::ResolvedTarget,
    repo: &str,
    auth: &crate::credentials::ApiAuth,
) -> eyre::Result<ActionsClient> {
    let api = ApiActions::connect(target, repo, auth).await?;
    Ok(ActionsClient::Api(Box::new(api)))
}

impl ActionsClient {
    pub async fn list_workflows(
        &self,
        repo: &str,
        page: u32,
        limit: u32,
    ) -> eyre::Result<Vec<String>> {
        match self {
            Self::Ui(s) => crate::ui_actions::list_workflows(s, repo, page, limit).await,
            Self::Api(api) => api.list_workflows(page, limit).await,
        }
    }

    pub async fn list_runs(
        &self,
        repo: &str,
        workflow: Option<&str>,
        page: u32,
        limit: u32,
    ) -> eyre::Result<Vec<RunRef>> {
        match self {
            Self::Ui(s) => crate::ui_actions::list_runs(s, repo, workflow, page, limit).await,
            Self::Api(api) => api.list_runs(workflow, page, limit).await,
        }
    }

    pub async fn latest_run_index_optional(
        &self,
        repo: &str,
        workflow: Option<&str>,
    ) -> eyre::Result<Option<i64>> {
        Ok(self
            .list_runs(repo, workflow, 1, 1)
            .await?
            .first()
            .map(|r| r.run_index))
    }

    pub async fn latest_run_index(&self, repo: &str, workflow: Option<&str>) -> eyre::Result<i64> {
        match self {
            Self::Ui(s) => crate::ui_actions::latest_run_index(s, repo, workflow).await,
            Self::Api(api) => api
                .latest_run_index(workflow)
                .await?
                .ok_or_else(|| eyre!("No action runs found for {repo}.")),
        }
    }

    /// Authoritative run status (the UI's run list, or the API run object).
    pub async fn run_status(&self, repo: &str, run_index: i64) -> eyre::Result<Option<String>> {
        match self {
            Self::Ui(s) => Ok(crate::ui_actions::list_runs(s, repo, None, 1, 50)
                .await?
                .into_iter()
                .find(|run| run.run_index == run_index)
                .and_then(|run| run.status)),
            Self::Api(api) => api.run_status(run_index).await,
        }
    }

    pub async fn run_jobs(&self, repo: &str, run_index: i64) -> eyre::Result<Vec<JobInfo>> {
        match self {
            Self::Ui(s) => {
                let view = crate::ui_actions::get_run_view_data(s, repo, run_index)
                    .await
                    .wrap_err("failed to load run view")?;
                crate::ui_actions::get_run_jobs(run_index, &view.view)
            }
            Self::Api(api) => api.run_jobs(run_index).await,
        }
    }

    pub async fn job_attempt(
        &self,
        repo: &str,
        run_index: i64,
        job_index: i64,
    ) -> eyre::Result<i64> {
        match self {
            Self::Ui(s) => Ok(
                crate::ui_actions::get_job_view_meta(s, repo, run_index, job_index)
                    .await?
                    .attempt_number,
            ),
            Self::Api(api) => api.job_attempt(run_index, job_index).await,
        }
    }

    pub async fn open_job_logs(
        &self,
        repo: &str,
        run_index: i64,
        job_index: i64,
        attempt: i64,
    ) -> eyre::Result<reqwest::Response> {
        match self {
            Self::Ui(s) => {
                crate::ui_actions::open_job_logs(s, repo, run_index, job_index, attempt).await
            }
            Self::Api(api) => api.open_job_logs(run_index, job_index, attempt).await,
        }
    }

    pub async fn run_artifacts(&self, repo: &str, run_index: i64) -> eyre::Result<Value> {
        match self {
            Self::Ui(s) => crate::ui_actions::get_run_artifacts(s, repo, run_index).await,
            Self::Api(api) => api.run_artifacts(run_index).await,
        }
    }

    pub async fn open_artifact(
        &self,
        repo: &str,
        run_index: i64,
        name_or_id: &str,
    ) -> eyre::Result<reqwest::Response> {
        match self {
            Self::Ui(s) => crate::ui_actions::open_artifact(s, repo, run_index, name_or_id).await,
            Self::Api(api) => api.open_artifact(run_index, name_or_id).await,
        }
    }

    pub fn run_url(&self, repo: &str, run_index: i64) -> String {
        match self {
            Self::Ui(s) => format!(
                "{}/{}/actions/runs/{run_index}",
                s.base_url(),
                repo.trim_matches('/')
            ),
            Self::Api(api) => api.run_url(run_index),
        }
    }

    pub async fn cancel_url(&self, repo: &str, run_index: i64) -> eyre::Result<String> {
        match self {
            Self::Ui(_) => Ok(format!("{}/cancel", self.run_url(repo, run_index))),
            Self::Api(api) => api.cancel_url(run_index).await,
        }
    }

    pub async fn cancel(&self, repo: &str, run_index: i64) -> eyre::Result<String> {
        match self {
            Self::Ui(s) => {
                let url = self.cancel_url(repo, run_index).await?;
                let resp = s
                    .post_json_response(&url, &serde_json::json!({}), true)
                    .await?;
                if resp.status() != reqwest::StatusCode::OK {
                    return Err(eyre!(
                        "Failed to cancel run '{}' via '{}'. HTTP {}.",
                        run_index,
                        url,
                        resp.status()
                    ));
                }
                Ok(url)
            }
            Self::Api(api) => api.cancel(run_index).await,
        }
    }

    fn ui_rerun_url(&self, repo: &str, run_index: i64, job_index: Option<i64>) -> String {
        let run_url = self.run_url(repo, run_index);
        match job_index {
            Some(job_index) => format!("{run_url}/jobs/{job_index}/rerun"),
            None => format!("{run_url}/rerun"),
        }
    }

    pub async fn rerun_url(
        &self,
        repo: &str,
        run_index: i64,
        job_index: Option<i64>,
    ) -> eyre::Result<String> {
        match self {
            Self::Ui(_) => Ok(self.ui_rerun_url(repo, run_index, job_index)),
            Self::Api(api) => api.rerun_url(run_index, job_index).await,
        }
    }

    /// Request a rerun. Returns the request URL and, for the UI, the redirect it returned.
    pub async fn rerun(
        &self,
        repo: &str,
        run_index: i64,
        job_index: Option<i64>,
    ) -> eyre::Result<(String, Option<String>)> {
        match self {
            Self::Ui(s) => {
                let url = self.ui_rerun_url(repo, run_index, job_index);
                let resp = s
                    .post_json_response(&url, &serde_json::json!({}), true)
                    .await?;
                if resp.status() != reqwest::StatusCode::OK {
                    return Err(eyre!(
                        "Failed to rerun via '{}'. HTTP {}.",
                        url,
                        resp.status()
                    ));
                }
                let text = resp.text().await.unwrap_or_default();
                Ok((url, redirect_from_body(&text)))
            }
            Self::Api(api) => Ok((api.rerun(run_index, job_index).await?, None)),
        }
    }

    /// Fail early when the backend cannot rerun failed jobs on this server.
    pub fn check_rerun_failed_supported(&self) -> eyre::Result<()> {
        match self {
            Self::Ui(_) => Ok(()),
            Self::Api(api) => api.require(crate::api_actions::ApiOp::RerunFailed),
        }
    }

    /// The POST requests a failed-jobs rerun would make.
    pub async fn rerun_failed_plan(
        &self,
        repo: &str,
        run_index: i64,
        failed_job_indexes: &[i64],
    ) -> eyre::Result<Vec<String>> {
        match self {
            Self::Ui(_) => Ok(failed_job_indexes
                .iter()
                .map(|job_index| self.ui_rerun_url(repo, run_index, Some(*job_index)))
                .collect()),
            Self::Api(api) => api.rerun_failed_plan(run_index, failed_job_indexes).await,
        }
    }

    pub async fn rerun_failed(
        &self,
        repo: &str,
        run_index: i64,
        failed_job_indexes: &[i64],
    ) -> eyre::Result<RerunFailedResult> {
        match self {
            Self::Ui(s) => {
                let mut failures = Vec::new();
                let mut redirects = Vec::new();
                for job_index in failed_job_indexes {
                    let url = self.ui_rerun_url(repo, run_index, Some(*job_index));
                    let resp = match s
                        .post_json_response(&url, &serde_json::json!({}), true)
                        .await
                    {
                        Ok(resp) => resp,
                        Err(e) => {
                            failures.push(format!(
                                "job {}: POST {} -> transport error: {}",
                                job_index, url, e
                            ));
                            continue;
                        }
                    };
                    if resp.status() != reqwest::StatusCode::OK {
                        failures.push(format!(
                            "job {}: POST {} -> HTTP {}",
                            job_index,
                            url,
                            resp.status()
                        ));
                        continue;
                    }
                    let text = resp.text().await.unwrap_or_default();
                    if let Some(redirect) = redirect_from_body(&text) {
                        redirects.push(redirect);
                    }
                }
                Ok(RerunFailedResult {
                    failures,
                    redirects,
                })
            }
            Self::Api(api) => {
                let outcome = api.rerun_failed(run_index, failed_job_indexes).await?;
                Ok(RerunFailedResult {
                    failures: outcome.failures,
                    redirects: Vec::new(),
                })
            }
        }
    }
}

fn redirect_from_body(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(text)
        .ok()?
        .get("redirect")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_keeps_ui_when_a_ui_login_is_stored() {
        assert_eq!(auto_choice(true, true), AutoChoice::Ui);
        assert_eq!(auto_choice(true, false), AutoChoice::Ui);
        assert_eq!(auto_choice(false, true), AutoChoice::Api);
        assert_eq!(auto_choice(false, false), AutoChoice::None);
    }

    #[test]
    fn rerun_redirects_are_read_from_json_bodies() {
        assert_eq!(
            redirect_from_body(r#"{"redirect":"/o/r/actions/runs/2"}"#).as_deref(),
            Some("/o/r/actions/runs/2")
        );
        assert_eq!(redirect_from_body(""), None);
        assert_eq!(redirect_from_body("<html>"), None);
    }
}
