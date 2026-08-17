mod actions;
mod api;
mod auth;
mod cli;
mod html;
mod login;
mod output;
mod pulls;
mod session;
mod session_cookies;
mod smoke_test;
mod store;
mod target;
mod token;
mod ui_actions;

use clap::Parser;
use cli::{App, Command};
use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;

fn main() -> ExitCode {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if !is_broken_pipe_payload(info.payload()) {
            default_hook(info);
        }
    }));

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to initialize async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match panic::catch_unwind(AssertUnwindSafe(|| runtime.block_on(run()))) {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            eprintln!("{error:?}");
            ExitCode::FAILURE
        }
        // println! panics after a downstream consumer such as `head` closes its
        // pipe. That is a successful early-consumer exit, not a CLI failure.
        Err(payload) if is_broken_pipe_payload(payload.as_ref()) => ExitCode::SUCCESS,
        Err(payload) => panic::resume_unwind(payload),
    }
}

async fn run() -> eyre::Result<()> {
    let app = App::parse();
    match app.command {
        Command::Auth(args) => auth::run(args).await,
        Command::Token(args) => token::run(args).await,
        Command::Login(args) => auth::run_legacy_login(args).await,
        Command::Actions(args) => actions::run(args).await,
        Command::Pulls(args) => pulls::run(args).await,
        Command::SmokeTest(args) => smoke_test::run(args).await,
    }
}

fn is_broken_pipe_payload(payload: &(dyn Any + Send)) -> bool {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    message.is_some_and(|message| message.contains("Broken pipe"))
}

#[cfg(test)]
mod tests {
    use super::is_broken_pipe_payload;

    #[test]
    fn detects_stdio_broken_pipe_panic() {
        let payload: Box<dyn std::any::Any + Send> =
            Box::new("failed printing to stdout: Broken pipe (os error 32)".to_string());
        assert!(is_broken_pipe_payload(payload.as_ref()));
        assert!(!is_broken_pipe_payload(&"other panic"));
    }
}
