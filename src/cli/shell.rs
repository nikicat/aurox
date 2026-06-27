//! Interactive shell (REPL) for the no-arg `gaur` invocation.
//!
//! A persistent prompt the user drives with word-commands (`search`, `add`,
//! `upgrade`, `apply`, …) against long-lived session state, replacing the
//! wizard-style `dialoguer` flows. See `docs/plans/shell-ui.md` for the full
//! design and phasing.
//!
//! **Phase 1 (this commit) is the REPL skeleton:** line editing + history, the
//! [`command`] parser, control-flow plumbing (`help`/`quit`/Ctrl-C/Ctrl-D), and
//! the [`ShellEnv`]/[`dispatch`] split that makes command handling unit-testable
//! with a scripted fake (mirroring the `LoopEnv`/`drive` pattern in
//! [`crate::cli::upgrade_loop`]).
//!
//! Bare interactive `gaur` enters the shell. The cart-staging verbs
//! (`add`/`show`/`apply`/…) are acknowledged stubs until later phases wire the
//! session, cart, and apply engine. `upgrade` already works: in phase 1 it
//! bridges to the existing upgrade loop, so the headline no-arg flow doesn't
//! regress while the cart-based path is built out (phases 3–4 replace the
//! bridge).

use crate::cli::upgrade_loop;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths;
use command::Command;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tracing::{debug, info, instrument};

pub mod command;

/// Control-flow result of dispatching one command.
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep reading commands.
    Continue,
    /// Leave the shell with this process exit code.
    Exit(u8),
}

/// The side-effecting operations command dispatch needs.
///
/// Behind a trait so the pure control flow ([`dispatch`]) is unit-testable with
/// a scripted fake. Phase 1 needs user-facing output plus the upgrade bridge;
/// later phases grow this with the session, cart, and build operations.
pub trait ShellEnv {
    /// Emit one line of user-facing output.
    fn print(&mut self, line: &str);
    /// Run an upgrade pass. Phase 1 delegates to the existing upgrade loop;
    /// phases 3–4 replace this with cart-based staging.
    fn upgrade(&mut self) -> Result<()>;
}

/// Pure command dispatch: map a parsed [`Command`] to side effects (through
/// `env`) + control flow. Does no I/O of its own, so the command surface and
/// exit conditions are testable without a terminal.
pub fn dispatch<E: ShellEnv>(cmd: &Command, env: &mut E) -> Flow {
    match cmd {
        Command::Empty => Flow::Continue,
        Command::Quit => Flow::Exit(0),
        Command::Syntax(msg) => {
            env.print(&format!("syntax error: {msg}"));
            Flow::Continue
        }
        Command::Unknown(verb) => {
            env.print(&format!(
                "unknown command `{verb}` — type `help` for the command list"
            ));
            Flow::Continue
        }
        Command::Help(_topic) => {
            env.print(HELP_TEXT);
            Flow::Continue
        }
        Command::Upgrade(args) => {
            if !args.is_empty() {
                env.print("note: per-package upgrade filtering arrives in a later phase; running the full upgrade");
            }
            if let Err(e) = env.upgrade() {
                env.print(&format!("upgrade: {e}"));
            }
            Flow::Continue
        }
        // The cart-staging verbs aren't wired up in the phase-1 skeleton; they
        // arrive in later phases. Acknowledge them so the surface is visible
        // and testable now rather than silently no-op'ing.
        other => {
            env.print(&format!(
                "`{}` isn't implemented yet — phase 1 is the REPL skeleton (see docs/plans/shell-ui.md)",
                other.verb()
            ));
            Flow::Continue
        }
    }
}

/// The `help` command body. A flat command list in phase 1; per-command topics
/// land with the commands themselves.
const HELP_TEXT: &str = "\
commands:
  search <terms…>     find packages (repo + AUR)
  info <pkg>          show package details
  add <pkg…>          stage packages to install
  drop <pkg…>         unstage packages from the cart
  remove <pkg…>       stage packages to uninstall
  upgrade [pkg…]      upgrade installed packages (repo + AUR)
  review <pkg>        view a PKGBUILD/diff and approve it
  show                preview the staged transaction
  apply               build + install the staged transaction
  clear               empty the cart
  refresh             re-fetch the AUR mirror + index
  help                this list
  quit                leave the shell (also: Ctrl-D)
note: only `upgrade` does real work in this build (phase 1 — REPL skeleton).";

/// Run the interactive shell. Returns the desired process exit code.
#[instrument(skip(cfg))]
pub fn run(cfg: &Config, devel: bool) -> Result<u8> {
    info!(devel, "shell session start");
    let mut env = RealEnv { cfg, devel };
    env.print("gitaur shell — type `help` for commands, `quit` to leave");

    let mut rl =
        DefaultEditor::new().map_err(|e| Error::other(format!("shell: init line editor: {e}")))?;
    let history = paths::shell_history_path();
    // A missing history file on first run is expected, not an error.
    rl.load_history(&history).ok();

    let code = loop {
        match rl.readline("gaur> ") {
            Ok(line) => {
                if !line.trim().is_empty() {
                    // Best-effort: a full history ring shouldn't abort input.
                    rl.add_history_entry(line.as_str()).ok();
                }
                if let Flow::Exit(code) = dispatch(&command::parse(&line), &mut env) {
                    break code;
                }
            }
            // Ctrl-C cancels the current line; it does NOT leave the shell.
            Err(ReadlineError::Interrupted) => {}
            // Ctrl-D at the prompt exits cleanly.
            Err(ReadlineError::Eof) => break 0,
            Err(e) => return Err(Error::other(format!("shell: read line: {e}"))),
        }
    };

    // History persistence is best-effort: a save failure shouldn't fail the run.
    if let Err(e) = rl.save_history(&history) {
        debug!(error = %e, "shell: could not save history");
    }
    Ok(code)
}

/// Production [`ShellEnv`]: writes to stdout and bridges `upgrade` to the
/// existing loop.
struct RealEnv<'a> {
    cfg: &'a Config,
    devel: bool,
}

impl ShellEnv for RealEnv<'_> {
    fn print(&mut self, line: &str) {
        println!("{line}");
    }

    fn upgrade(&mut self) -> Result<()> {
        // The loop returns its own exit code; inside the shell we only care
        // whether it errored — control returns to the prompt either way.
        upgrade_loop::run(self.cfg, self.devel).map(|_code| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted [`ShellEnv`] capturing printed lines + upgrade calls so tests
    /// can assert what dispatch produced without a terminal or a real upgrade.
    #[derive(Default)]
    struct FakeEnv {
        lines: Vec<String>,
        upgrades: usize,
    }

    impl ShellEnv for FakeEnv {
        fn print(&mut self, line: &str) {
            self.lines.push(line.to_owned());
        }
        fn upgrade(&mut self) -> Result<()> {
            self.upgrades += 1;
            Ok(())
        }
    }

    fn dispatch_one(input: &str) -> (Flow, FakeEnv) {
        let mut env = FakeEnv::default();
        let flow = dispatch(&command::parse(input), &mut env);
        (flow, env)
    }

    #[test]
    fn quit_and_aliases_exit_zero() {
        assert_eq!(dispatch_one("quit").0, Flow::Exit(0));
        assert_eq!(dispatch_one("exit").0, Flow::Exit(0));
        assert_eq!(dispatch_one("q").0, Flow::Exit(0));
    }

    #[test]
    fn empty_line_continues_with_no_output() {
        let (flow, env) = dispatch_one("   ");
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines.is_empty(),
            "blank line prints nothing: {:?}",
            env.lines
        );
    }

    #[test]
    fn unknown_command_points_at_help() {
        let (flow, env) = dispatch_one("frobnicate x");
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines
                .iter()
                .any(|l| l.contains("unknown command") && l.contains("frobnicate")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn help_lists_the_core_verbs() {
        let (flow, env) = dispatch_one("help");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.join("\n");
        for verb in ["search", "add", "upgrade", "apply", "quit"] {
            assert!(joined.contains(verb), "help text missing `{verb}`");
        }
    }

    #[test]
    fn upgrade_bridges_to_the_loop_and_continues() {
        let (flow, env) = dispatch_one("upgrade");
        assert_eq!(flow, Flow::Continue);
        assert_eq!(
            env.upgrades, 1,
            "upgrade should call the bridge exactly once"
        );
    }

    #[test]
    fn upgrade_with_args_notes_then_still_runs() {
        let (flow, env) = dispatch_one("upgrade firefox");
        assert_eq!(flow, Flow::Continue);
        assert_eq!(env.upgrades, 1);
        assert!(
            env.lines.iter().any(|l| l.contains("later phase")),
            "should note that filtering isn't wired yet: {:?}",
            env.lines
        );
    }

    #[test]
    fn cart_verbs_are_acknowledged_stubs() {
        for input in ["search firefox", "add foo", "show", "apply"] {
            let (flow, env) = dispatch_one(input);
            assert_eq!(flow, Flow::Continue, "stub should continue: {input}");
            assert!(
                env.lines
                    .iter()
                    .any(|l| l.contains("isn't implemented yet")),
                "stub for `{input}` should acknowledge itself: {:?}",
                env.lines
            );
        }
    }

    #[test]
    fn syntax_error_is_reported_not_fatal() {
        let (flow, env) = dispatch_one("add \"unterminated");
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines.iter().any(|l| l.contains("syntax error")),
            "got: {:?}",
            env.lines
        );
    }
}
