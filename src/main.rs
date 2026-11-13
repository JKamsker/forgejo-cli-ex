mod actions;
mod actions_backend;
mod api;
mod api_actions;
mod auth;
mod cli;
mod credentials;
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
        // Streamed output (job logs) is written with async writes, which
        // report a closed pipe as an error instead of panicking.
        Ok(Err(error)) if is_closed_output_error(&error) => ExitCode::SUCCESS,
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

/// Whether an error is a broken pipe from writing our own output, as opposed
/// to an HTTP connection (whose I/O errors are wrapped in `reqwest::Error`).
fn is_closed_output_error(error: &eyre::Report) -> bool {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
    {
        return false;
    }
    error
        .root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

fn is_broken_pipe_payload(payload: &(dyn Any + Send)) -> bool {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    message.is_some_and(is_stdio_broken_pipe_message)
}

/// Matches only the panic raised by `print!`/`eprint!` (and friends) when the
/// reader of a standard stream has gone away, e.g.
/// `failed printing to stdout: Broken pipe (os error 32)`. Unrelated panics
/// that merely mention a broken pipe must still surface as failures.
fn is_stdio_broken_pipe_message(message: &str) -> bool {
    ["failed printing to stdout: ", "failed printing to stderr: "]
        .iter()
        .filter_map(|prefix| message.strip_prefix(prefix))
        .any(|io_error| io_error.starts_with("Broken pipe"))
}

#[cfg(test)]
mod tests {
    use super::{is_broken_pipe_payload, is_closed_output_error};
    use eyre::WrapErr;

    #[test]
    fn treats_a_closed_output_pipe_as_success() {
        let broken_pipe = || std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(is_closed_output_error(&eyre::Report::new(broken_pipe())));
        let wrapped: eyre::Result<()> =
            Err(broken_pipe()).wrap_err("failed to write response body chunk");
        assert!(is_closed_output_error(&wrapped.unwrap_err()));

        let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_closed_output_error(&eyre::Report::new(other)));
        assert!(!is_closed_output_error(&eyre::eyre!("Broken pipe")));
    }

    #[test]
    fn detects_stdio_broken_pipe_panic() {
        let payload: Box<dyn std::any::Any + Send> =
            Box::new("failed printing to stdout: Broken pipe (os error 32)".to_string());
        assert!(is_broken_pipe_payload(payload.as_ref()));
        assert!(is_broken_pipe_payload(
            &"failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(!is_broken_pipe_payload(&"other panic"));
    }

    #[test]
    fn ignores_unrelated_panics_that_mention_broken_pipe() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(
            "called `Result::unwrap()` on an `Err` value: Broken pipe (os error 32)".to_string(),
        );
        assert!(!is_broken_pipe_payload(payload.as_ref()));
        assert!(!is_broken_pipe_payload(
            &"failed writing cache: Broken pipe (os error 32)"
        ));
        assert!(!is_broken_pipe_payload(
            &"failed printing to stdout: formatter error"
        ));
    }
}
