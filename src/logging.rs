//! Tracing subscriber setup.
//!
//! Three layers fed off the same `#[instrument]` spans and events:
//! - a console layer (env-filter, default `warn`) on stderr;
//! - a per-invocation text log at `debug` in `state_dir()/logs/`;
//! - a per-invocation Chrome/Perfetto span trace in `state_dir()/traces/`,
//!   capturing the span hierarchy + timings (git fetch, parallel index
//!   rebuild) as a flamegraph — drag the `.json` into
//!   <https://ui.perfetto.dev> (or `chrome://tracing`) to inspect it.
//!
//! Old files in both directories are pruned to the newest few on every startup
//! via the [`Logs`] / [`Traces`] [`RotationPolicy`] implementations.

use std::path::PathBuf;
use std::sync::Mutex;

use tracing_chrome::ChromeLayerBuilder;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

use crate::paths;
use crate::rotate::{self, RotationPolicy};

/// File-layer default filter. Baseline is `debug` so gix-progress state
/// changes (`set_name`, `add_child`, `message`) land in the log, but the very
/// chatty per-percent `trace!` events do not. Per-crate overrides silence the
/// HTTP-plumbing layers (h2 frame-by-frame, hyper connection pool, rustls
/// platform verifier, reqwest connect) which otherwise drown gitaur's own
/// events ~5:1 during a single fetch.
const FILE_LOG_FILTER: &str = "debug,h2=info,hyper=info,hyper_util=info,reqwest=info,rustls=info,rustls_platform_verifier=info";

/// Keep-alive for the run's tracing resources.
///
/// Wraps the [`tracing_chrome`] flush guard, which finalizes the trace file on
/// drop (it buffers events on a background thread and writes the closing JSON
/// when dropped). `main` must bind this for the whole run — dropping it early
/// truncates the trace.
#[must_use = "dropping the guard flushes the trace file; keep it alive for the whole run"]
pub struct Guard {
    /// Flushes + closes the trace file on drop. Held, never read.
    _chrome: Option<tracing_chrome::FlushGuard>,
}

/// Initialize tracing. Returns a [`Guard`] that must be kept alive for the
/// duration of the run (see its docs).
///
/// Best-effort: console logging always works; if a log or trace file can't be
/// created we print a warning to stderr and continue without that sink.
pub fn init() -> Guard {
    let console_filter = parse_console_filter(std::env::var("RUST_LOG"), &mut std::io::stderr());
    // `fmt::layer()` defaults to stdout, which competes with subprocess
    // stdout (makepkg, pacman -U). Pin to stderr so log lines interleave
    // cleanly with `ui::{step,note,…}` (which all use eprintln) and don't
    // pollute callers that capture gitaur's stdout.
    let console_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(console_filter);

    // One stem per run, shared by the log and trace files so they correlate.
    let basename = rotate::run_basename();

    let (file_layer, log_path) = match Logs.create(&basename) {
        Ok((file, path)) => {
            let layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_timer(JiffTimer)
                .with_writer(Mutex::new(file))
                .with_filter(EnvFilter::new(FILE_LOG_FILTER));
            (Some(layer), Some(path))
        }
        Err(e) => {
            eprintln!("gitaur: file logging disabled: {e}");
            (None, None)
        }
    };

    // Chrome trace layer: same span/event stream as the file log, but emitted
    // as trace-events. `include_args(true)` folds the `#[instrument]`
    // `fields(...)` (branch, counts, version, …) into each span's args so they
    // show up in the Perfetto detail pane.
    //
    // `TraceStyle::Async` (not the default Threaded): it records a span on
    // open→close (`on_new_span`/`on_close`) and nests by parent scope, rather
    // than on enter/exit per thread. That's what lets the gix fetch-phase
    // sub-spans work — they're *held, not entered* (the progress adapter must
    // stay `Send + Sync`, so it can't keep an `EnteredSpan`), and it keeps
    // spans that open and close on different threads intact.
    let (chrome_layer, chrome_guard, trace_path) = match Traces.create(&basename) {
        Ok((file, path)) => {
            let (layer, guard) = ChromeLayerBuilder::new()
                .writer(file)
                .include_args(true)
                .trace_style(tracing_chrome::TraceStyle::Async)
                .build();
            (
                Some(layer.with_filter(EnvFilter::new(FILE_LOG_FILTER))),
                Some(guard),
                Some(path),
            )
        }
        Err(e) => {
            eprintln!("gitaur: span tracing disabled: {e}");
            (None, None, None)
        }
    };

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .with(chrome_layer)
        .init();

    if let Some(path) = &log_path {
        tracing::debug!(path = %path.display(), "execution log opened");
        Logs.prune();
    }
    if let Some(path) = &trace_path {
        tracing::debug!(path = %path.display(), "span trace opened");
        Traces.prune();
    }

    Guard {
        _chrome: chrome_guard,
    }
}

/// The per-run text log in `state_dir()/logs/` (`gitaur-*.log`).
struct Logs;

impl RotationPolicy for Logs {
    fn dir(&self) -> PathBuf {
        paths::logs_dir()
    }
    fn ext(&self) -> &'static str {
        "log"
    }
    fn keep(&self) -> usize {
        10
    }
}

/// The per-run Chrome/Perfetto span trace in `state_dir()/traces/`
/// (`gitaur-*.json`). Kept lower than logs because trace JSON is far larger.
struct Traces;

impl RotationPolicy for Traces {
    fn dir(&self) -> PathBuf {
        paths::traces_dir()
    }
    fn ext(&self) -> &'static str {
        "json"
    }
    fn keep(&self) -> usize {
        10
    }
}

/// Translate a `RUST_LOG` env-var lookup into a console-layer [`EnvFilter`].
///
/// The opaque `FromEnvError` from `EnvFilter::try_from_default_env` doesn't
/// tell us *why* the lookup failed, so we branch on the raw [`Result`] from
/// `env::var`: only the "unset" path falls back silently — anything else (bad
/// UTF-8, malformed directive) is the user typing something we have to
/// ignore, and we tell them via `diag` so a typo doesn't silently kill their
/// debug output. `diag` is `&mut dyn Write` so callers can inject stderr (the
/// production wiring) or a `Vec<u8>` (tests).
fn parse_console_filter(
    raw: Result<String, std::env::VarError>,
    diag: &mut dyn std::io::Write,
) -> EnvFilter {
    match raw {
        Err(std::env::VarError::NotPresent) => EnvFilter::new("warn"),
        Err(std::env::VarError::NotUnicode(_)) => {
            writeln!(
                diag,
                "gitaur: RUST_LOG is not valid UTF-8; falling back to RUST_LOG=warn",
            )
            .ok();
            EnvFilter::new("warn")
        }
        Ok(raw) => EnvFilter::try_new(&raw).unwrap_or_else(|e| {
            writeln!(
                diag,
                "gitaur: ignoring malformed RUST_LOG='{raw}' ({e}); falling back to RUST_LOG=warn",
            )
            .ok();
            EnvFilter::new("warn")
        }),
    }
}

struct JiffTimer;

impl FormatTime for JiffTimer {
    fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn policies_carry_their_own_extension() {
        // The retention/creation mechanics are covered in `crate::rotate`;
        // here we just pin the per-family wiring the two policies supply.
        assert_eq!(Logs.ext(), "log");
        assert_eq!(Traces.ext(), "json");
        // `owns` (trait-provided) keys off that extension.
        assert!(Logs.owns(OsStr::new("gitaur-x.log")));
        assert!(!Logs.owns(OsStr::new("gitaur-x.json")));
        assert!(Traces.owns(OsStr::new("gitaur-x.json")));
    }

    #[test]
    fn parse_filter_falls_back_silently_when_unset() {
        let mut diag = Vec::<u8>::new();
        let f = parse_console_filter(Err(std::env::VarError::NotPresent), &mut diag);
        assert_eq!(f.to_string(), "warn");
        assert!(
            diag.is_empty(),
            "unset RUST_LOG must not produce diagnostics",
        );
    }

    #[test]
    fn parse_filter_warns_on_non_utf8() {
        let mut diag = Vec::<u8>::new();
        let bad = std::ffi::OsString::from("warn");
        // VarError::NotUnicode takes an OsString — we don't care what's inside,
        // only that this variant routes to the warn-then-fallback branch.
        let f = parse_console_filter(Err(std::env::VarError::NotUnicode(bad)), &mut diag);
        assert_eq!(f.to_string(), "warn");
        let msg = String::from_utf8(diag).unwrap();
        assert!(msg.contains("not valid UTF-8"), "got: {msg}");
    }

    #[test]
    fn parse_filter_warns_on_malformed_directive() {
        let mut diag = Vec::<u8>::new();
        // `brbug` is not a known level (the levels are trace/debug/info/warn/
        // error/off). EnvFilter rejects unknown level names.
        let f = parse_console_filter(Ok("mycrate=brbug".into()), &mut diag);
        assert_eq!(f.to_string(), "warn");
        let msg = String::from_utf8(diag).unwrap();
        assert!(msg.contains("malformed RUST_LOG"), "got: {msg}");
        assert!(
            msg.contains("mycrate=brbug"),
            "diag should echo the bad value: {msg}",
        );
    }

    #[test]
    fn parse_filter_accepts_valid_directives() {
        let mut diag = Vec::<u8>::new();
        // Multi-directive parses cleanly; we don't pin the exact serialization
        // (EnvFilter reorders directives alphabetically) — only that it didn't
        // hit the diagnostic branch.
        let _f = parse_console_filter(Ok("info,h2=warn".into()), &mut diag);
        assert!(
            diag.is_empty(),
            "valid directives must not produce diagnostics"
        );
    }
}
