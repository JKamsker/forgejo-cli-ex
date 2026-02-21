use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "fj-ex", version, about)]
pub struct App {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Authentication and credential management (UI session cookies + plaintext creds).
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Forgejo Actions: workflows, runs, jobs, logs, artifacts, cancel/rerun.
    Actions(ActionsCommand),
    /// Smoke test for Actions access (useful for debugging auth/connectivity/log downloads).
    #[command(name = "smoke-test")]
    SmokeTest(SmokeTestCommand),
    /// Legacy alias for `fj-ex auth login`.
    #[command(hide = true)]
    Login(LoginCommand),
}

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Log in to an instance (stores plaintext creds + UI session cookies).
    Login(LoginCommand),
    /// Check login status against the host.
    Status(AuthStatusCommand),
    /// List all stored logins.
    List(AuthListCommand),
    /// Show stored login info for a host.
    Show(AuthShowCommand),
    /// Delete stored login info for a host.
    Logout(AuthLogoutCommand),
    /// Clear stored UI cookies for a host (keeps creds).
    #[command(name = "clear-cookies")]
    ClearCookies(AuthClearCookiesCommand),
}

#[derive(Args, Debug, Clone)]
pub struct TargetArgs {
    /// Forgejo host or base URL (e.g. forge.example.com or https://forge.example.com)
    #[arg(long, short = 'H')]
    pub host: Option<String>,

    /// Repo to operate on (owner/name or host/owner/name)
    #[arg(long, short = 'r')]
    pub repo: Option<crate::target::RepoArg>,

    /// Local git remote used to infer host/repo (default: origin)
    #[arg(long, short = 'R')]
    pub remote: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LoginCommand {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Credentials as user:pass (unsafe: visible in process list)
    #[arg(long, conflicts_with_all = ["username", "password", "password_stdin"])]
    pub userpass: Option<String>,

    /// Username to login with (falls back to FJ_USER)
    #[arg(long)]
    pub username: Option<String>,

    /// Password to login with (unsafe: visible in process list; prefer --password-stdin)
    #[arg(long, conflicts_with = "password_stdin")]
    pub password: Option<String>,

    /// Read password from stdin (first line; falls back to prompt if empty)
    #[arg(long, conflicts_with = "password")]
    pub password_stdin: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AuthStatusCommand {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Don't re-login if the cookie session is invalid.
    #[arg(long)]
    pub no_relogin: bool,

    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AuthListCommand {
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AuthShowCommand {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Print JSON output.
    #[arg(long)]
    pub json: bool,

    /// UNSAFE: include the plaintext password in output.
    #[arg(long)]
    pub unsafe_show_password: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AuthLogoutCommand {
    #[command(flatten)]
    pub target: TargetArgs,
}

#[derive(Args, Debug, Clone)]
pub struct AuthClearCookiesCommand {
    #[command(flatten)]
    pub target: TargetArgs,
}

#[derive(Args, Debug, Clone)]
pub struct ActionsCommand {
    #[command(flatten)]
    pub target: TargetArgs,

    #[command(subcommand)]
    pub command: ActionsSubcommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ActionsSubcommand {
    /// List available workflows for the repo.
    Workflows {
        /// Print JSON output.
        #[arg(long, help = "Print JSON output.")]
        json: bool,
    },
    /// List workflow runs for the repo.
    Runs {
        #[arg(long, help = "Filter runs by workflow name.")]
        workflow: Option<String>,
        #[arg(long, default_value_t = 1, help = "Page number (1-based).")]
        page: u32,
        #[arg(long, default_value_t = 20, help = "Max runs per page.")]
        limit: u32,
        #[arg(long, help = "Print JSON output.")]
        json: bool,
    },
    /// List jobs for a run.
    #[command(group(
        clap::ArgGroup::new("run_selector")
            .required(true)
            .args(["run_index", "latest"])
    ))]
    Jobs {
        #[arg(long, help = "Run index to inspect.")]
        run_index: Option<i64>,
        #[arg(long, help = "Use the latest run.")]
        latest: bool,
        #[arg(long, help = "Print JSON output.")]
        json: bool,
    },
    /// Download and print logs.
    Logs {
        #[command(subcommand)]
        command: ActionsLogsSubcommand,
    },
    /// List or download run artifacts.
    Artifacts {
        #[command(subcommand)]
        command: ActionsArtifactsSubcommand,
    },
    /// Cancel a run.
    Cancel {
        #[arg(long, help = "Run index to cancel.")]
        run_index: i64,
        #[arg(long, help = "Print the request that would be made, but do not perform it.")]
        dry_run: bool,
    },
    /// Rerun a run (or a single job within a run).
    Rerun {
        #[arg(long, help = "Run index to rerun.")]
        run_index: i64,
        #[arg(long, help = "Rerun a specific job index within the run.")]
        job_index: Option<i64>,
        #[arg(long, help = "Print the request that would be made, but do not perform it.")]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ActionsLogsSubcommand {
    /// Download logs for all jobs in a run.
    ///
    /// Note: Job separators (`== job N ==`) are written to stderr; log content goes to stdout.
    #[command(group(
        clap::ArgGroup::new("run_selector")
            .required(true)
            .args(["run_index", "latest"])
    ))]
    Run {
        #[arg(long, help = "Run index to download logs for.")]
        run_index: Option<i64>,
        #[arg(long, help = "Use the latest run.")]
        latest: bool,
        #[arg(long, help = "Write per-job log files to this directory (otherwise stdout).")]
        out_dir: Option<std::path::PathBuf>,
        #[arg(
            long,
            default_value_t = 0,
            help = "Max jobs to download (0 = unlimited)."
        )]
        max_jobs: u32,
    },
    /// Download logs for a single job in a run.
    Job {
        #[arg(long, help = "Run index to download logs for.")]
        run_index: i64,
        #[arg(long, help = "Job index within the run.")]
        job_index: i64,
        #[arg(long, help = "Attempt number (defaults to latest attempt).")]
        attempt: Option<i64>,
        #[arg(long, help = "Write logs to this file (otherwise stdout).")]
        out_file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ActionsArtifactsSubcommand {
    /// List artifacts for a run.
    #[command(group(
        clap::ArgGroup::new("run_selector")
            .required(true)
            .args(["run_index", "latest"])
    ))]
    List {
        #[arg(long, help = "Run index to list artifacts for.")]
        run_index: Option<i64>,
        #[arg(long, help = "Use the latest run.")]
        latest: bool,
        #[arg(long, help = "Print JSON output.")]
        json: bool,
    },
    /// Download a single artifact from a run.
    Get {
        #[arg(long, help = "Run index to download the artifact from.")]
        run_index: i64,
        #[arg(long, help = "Artifact name or id.")]
        artifact: String,
        #[arg(long, help = "Output file path.")]
        out_file: std::path::PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SmokeTestCommand {
    #[command(flatten)]
    pub target: TargetArgs,

    #[arg(
        long,
        default_value_t = 1_048_576,
        help = "Max bytes to download per job log (default: 1 MiB)."
    )]
    pub log_download_max_bytes: u64,

    /// Base directory for smoke test log downloads (a run-specific folder is created inside)
    #[arg(long)]
    pub out_dir: Option<std::path::PathBuf>,
}
