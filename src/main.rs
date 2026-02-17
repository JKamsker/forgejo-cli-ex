mod actions;
mod cli;
mod login;
mod smoke_test;
mod target;
mod store;
mod html;
mod session;
mod ui_actions;

use clap::Parser;
use cli::{App, Command};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let app = App::parse();
    match app.command {
        Command::Login(args) => login::run(args).await,
        Command::Actions(args) => actions::run(args).await,
        Command::SmokeTest(args) => smoke_test::run(args).await,
    }
}
