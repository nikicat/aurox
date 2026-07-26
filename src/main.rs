//! `aurox` binary entry. Initializes tracing + dispatches to [`aurox::cli::run`].

use aurox::error::Error;
use aurox::{cli, logging, ui};
use std::process::ExitCode;

fn main() -> ExitCode {
    // Held for the whole run: dropping it flushes + closes the trace file.
    let _log_guard = logging::init();

    match cli::run() {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            // pacman decided this run's fate, so propagate *its* status rather
            // than flattening every failure to 1 — a script wrapping an aurox
            // pass-through sees what it would have seen calling pacman.
            match e {
                Error::PacmanExit(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
                _ => ExitCode::from(1),
            }
        }
    }
}
