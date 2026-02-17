use crate::cli::{
    AuthClearCookiesCommand, AuthCommand, AuthListCommand, AuthLogoutCommand, AuthShowCommand,
    AuthStatusCommand, LoginCommand,
};

pub async fn run(args: AuthCommand) -> eyre::Result<()> {
    match args {
        AuthCommand::Login(args) => crate::login::run(args).await,
        AuthCommand::Status(args) => run_status(args).await,
        AuthCommand::List(args) => run_list(args).await,
        AuthCommand::Show(args) => run_show(args).await,
        AuthCommand::Logout(args) => run_logout(args).await,
        AuthCommand::ClearCookies(args) => run_clear_cookies(args).await,
    }
}

pub async fn run_legacy_login(args: LoginCommand) -> eyre::Result<()> {
    eprintln!("warn: `fj-ex login` is deprecated; use `fj-ex auth login`.");
    crate::login::run(args).await
}

async fn run_status(_args: AuthStatusCommand) -> eyre::Result<()> {
    eyre::bail!("not implemented yet")
}

async fn run_list(_args: AuthListCommand) -> eyre::Result<()> {
    eyre::bail!("not implemented yet")
}

async fn run_show(_args: AuthShowCommand) -> eyre::Result<()> {
    eyre::bail!("not implemented yet")
}

async fn run_logout(_args: AuthLogoutCommand) -> eyre::Result<()> {
    eyre::bail!("not implemented yet")
}

async fn run_clear_cookies(_args: AuthClearCookiesCommand) -> eyre::Result<()> {
    eyre::bail!("not implemented yet")
}
