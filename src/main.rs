//! `gitaur` binary entry. Initializes tracing + dispatches to [`gitaur::cli::run`].

use gitaur::{cli, logging, ui};
use std::process::ExitCode;

fn main() -> ExitCode {
    logging::init();

    match cli::run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ExitCode::from(1)
        }
    }
}
