mod actions;
mod auth;
mod cli;
mod html;
mod login;
mod output;
mod session;
mod smoke_test;
mod store;
mod target;
mod ui_actions;

use clap::Parser;
use cli::{App, Command};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let app = App::parse();
    match app.command {
        Command::Auth(args) => auth::run(args).await,
        Command::Login(args) => auth::run_legacy_login(args).await,
        Command::Actions(args) => actions::run(args).await,
        Command::SmokeTest(args) => smoke_test::run(args).await,
    }
}
