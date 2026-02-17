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
    Actions(ActionsCommand),
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
    Workflows {
        #[arg(long)]
        json: bool,
    },
    Runs {
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    Jobs {
        #[arg(long)]
        run_index: Option<i64>,
        #[arg(long)]
        latest: bool,
        #[arg(long)]
        json: bool,
    },
    Logs {
        #[command(subcommand)]
        command: ActionsLogsSubcommand,
    },
    Artifacts {
        #[command(subcommand)]
        command: ActionsArtifactsSubcommand,
    },
    Cancel {
        #[arg(long)]
        run_index: i64,
        #[arg(long)]
        dry_run: bool,
    },
    Rerun {
        #[arg(long)]
        run_index: i64,
        #[arg(long)]
        job_index: Option<i64>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ActionsLogsSubcommand {
    Run {
        #[arg(long)]
        run_index: Option<i64>,
        #[arg(long)]
        latest: bool,
        #[arg(long)]
        out_dir: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = 0)]
        max_jobs: u32,
    },
    Job {
        #[arg(long)]
        run_index: i64,
        #[arg(long)]
        job_index: i64,
        #[arg(long)]
        attempt: Option<i64>,
        #[arg(long)]
        out_file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ActionsArtifactsSubcommand {
    List {
        #[arg(long)]
        run_index: Option<i64>,
        #[arg(long)]
        latest: bool,
        #[arg(long)]
        json: bool,
    },
    Get {
        #[arg(long)]
        run_index: i64,
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        out_file: std::path::PathBuf,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SmokeTestCommand {
    #[command(flatten)]
    pub target: TargetArgs,

    #[arg(long, default_value_t = 1_048_576)]
    pub log_download_max_bytes: u64,

    /// Base directory for smoke test log downloads (a run-specific folder is created inside)
    #[arg(long)]
    pub out_dir: Option<std::path::PathBuf>,
}
