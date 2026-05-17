//! `gitaur` binary entry. Initializes tracing + dispatches to [`gitaur::cli::run`].

use std::process::ExitCode;
use tracing_subscriber::{fmt, EnvFilter};

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt().with_env_filter(filter).with_target(false).init();

    match gitaur::cli::run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            gitaur::ui::error(&format!("{:#}", e));
            ExitCode::from(1)
        }
    }
}
