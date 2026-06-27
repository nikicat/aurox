//! Interactive shell (REPL) for the no-arg `gaur` invocation.
//!
//! A persistent prompt the user drives with word-commands (`search`, `add`,
//! `upgrade`, `apply`, …) against long-lived session state, replacing the
//! wizard-style `dialoguer` flows. See `docs/plans/shell-ui.md` for the full
//! design and phasing.
//!
//! **Phase 2 status:** the session is hoisted once at start (the AUR index +
//! secondary maps via [`UpgradeSession`], plus a sorted name universe for
//! globs/completion), and the read-only verbs work: `search` prints a numbered
//! result list the session remembers, and `info` shows package details by
//! number/name/range/glob via the [`selector`] core. `upgrade` bridges to the
//! existing loop (phase 1); the cart-staging verbs (`add`/`show`/`apply`/…)
//! remain acknowledged stubs until phase 3.
//!
//! The [`ShellEnv`]/[`dispatch`] split keeps command handling unit-testable
//! with a scripted fake (mirrors the `LoopEnv`/`drive` pattern in
//! [`crate::cli::upgrade_loop`]).

use crate::build::UpgradeSession;
use crate::cli::search::Row;
use crate::cli::upgrade_loop;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index;
use crate::names::{PkgTarget, SearchTerm};
use crate::pacman::alpm_db;
use crate::paths;
use crate::ui;
use command::Command;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tracing::{debug, info, instrument};

pub mod command;
pub mod selector;

/// One row of the most recent result list, addressable by its 1-based number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The thing `add` / `info` / … act on when this row is picked by number.
    pub target: PkgTarget,
    /// Preformatted display label (without the leading number).
    pub label: String,
}

/// Mutable per-session shell state the dispatch core threads between commands.
#[derive(Default)]
pub struct State {
    /// The last printed result list (`search`), addressable by number.
    last_list: Vec<ListItem>,
}

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
/// a scripted fake. Later phases grow this with the cart and build operations.
pub trait ShellEnv {
    /// Emit one line of user-facing output.
    fn print(&mut self, line: &str);
    /// Run an upgrade pass. Phase 1/2 delegate to the existing upgrade loop;
    /// phases 3–4 replace this with cart-based staging.
    fn upgrade(&mut self) -> Result<()>;
    /// Run a combined repo + AUR search; returns rows for the numbered list.
    fn search(&mut self, terms: &[SearchTerm]) -> Result<Vec<ListItem>>;
    /// Print `-Si`-style info for the already-resolved targets.
    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()>;
    /// Sorted universe of package names, for glob resolution + completion.
    fn names(&self) -> &[String];
}

/// Pure command dispatch: map a parsed [`Command`] to side effects + control
/// flow.
///
/// Side effects go through `env`/`state`; the function does no I/O of its own,
/// so the command surface and exit conditions are testable without a terminal.
pub fn dispatch<E: ShellEnv>(cmd: &Command, state: &mut State, env: &mut E) -> Flow {
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
        Command::Search(terms) => {
            handle_search(terms, state, env);
            Flow::Continue
        }
        Command::Info(args) => {
            handle_info(args, state, env);
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
        // The cart-staging verbs aren't wired up yet; they arrive in phase 3.
        // Acknowledge them so the surface is visible rather than silently
        // no-op'ing.
        other => {
            env.print(&format!(
                "`{}` isn't implemented yet (coming in a later phase — see docs/plans/shell-ui.md)",
                other.verb()
            ));
            Flow::Continue
        }
    }
}

/// `search <terms…>`: run the query, print a numbered list, remember it.
fn handle_search<E: ShellEnv>(terms: &[SearchTerm], state: &mut State, env: &mut E) {
    if terms.is_empty() {
        env.print("usage: search <terms…>");
        return;
    }
    match env.search(terms) {
        Ok(items) => {
            if items.is_empty() {
                let joined = terms
                    .iter()
                    .map(SearchTerm::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                env.print(&format!("no packages match `{joined}`"));
            } else {
                for (i, item) in items.iter().enumerate() {
                    env.print(&format!("{:>3}  {}", i + 1, item.label));
                }
            }
            // Replace the current list even when empty, so a stale list can't
            // be addressed by number after a fruitless search.
            state.last_list = items;
        }
        Err(e) => env.print(&format!("search: {e}")),
    }
}

/// `info <pkg|number|range|glob>…`: resolve the selectors and show details.
/// Reads the current list but doesn't mutate session state.
fn handle_info<E: ShellEnv>(args: &[String], state: &State, env: &mut E) {
    if args.is_empty() {
        env.print("usage: info <pkg|number|range|glob>…");
        return;
    }
    let targets = match selector::resolve(args, &state.last_list, env.names()) {
        Ok(t) => t,
        Err(e) => {
            env.print(&format!("info: {e}"));
            return;
        }
    };
    if targets.is_empty() {
        env.print("info: nothing matched");
        return;
    }
    if let Err(e) = env.show_info(&targets) {
        env.print(&format!("info: {e}"));
    }
}

/// The `help` command body. A flat command list; per-command topics land with
/// the commands themselves.
const HELP_TEXT: &str = "\
commands:
  search <terms…>     find packages (repo + AUR)
  info <sel…>         show package details (sel = name | number | range | glob)
  add <sel…>          stage packages to install
  drop <sel…>         unstage packages from the cart
  remove <sel…>       stage packages to uninstall
  upgrade [pkg…]      upgrade installed packages (repo + AUR)
  review <sel…>       view a PKGBUILD/diff and approve it
  show                preview the staged transaction
  apply               build + install the staged transaction
  clear               empty the cart
  refresh             re-fetch the AUR mirror + index
  help                this list
  quit                leave the shell (also: Ctrl-D)
selectors: `3` (row), `5-8` (range), `glibc` (name), `python-*` (glob)
note: search/info/upgrade work; the cart verbs (add/show/apply/…) are stubs.";

/// Run the interactive shell. Returns the desired process exit code.
#[instrument(skip(cfg))]
pub fn run(cfg: &Config, devel: bool) -> Result<u8> {
    info!(devel, "shell session start");
    // Once per session: load the AUR index (+ secondary maps) and the name
    // universe. Not repeated per command; `refresh` (later phase) re-fetches.
    let session = UpgradeSession::load(cfg)?;
    let names = build_name_universe(session.as_ref());
    debug!(
        names = names.len(),
        has_index = session.is_some(),
        "shell session loaded"
    );
    let mut env = RealEnv {
        cfg,
        devel,
        session,
        names,
    };
    let mut state = State::default();

    env.print("gitaur shell — type `help` for commands, `quit` to leave");
    if env.session.is_none() {
        env.print("no AUR index yet — run `gaur -Sy` to enable AUR search/info");
    }

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
                if let Flow::Exit(code) = dispatch(&command::parse(&line), &mut state, &mut env) {
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

/// Build the sorted, de-duplicated name universe: every AUR pkgname + pkgbase
/// from the index, plus sync-repo pkgnames (best-effort). Backs glob resolution
/// and, in a later phase, tab-completion. Built once per session; a missing
/// index or unreadable alpm just yields a smaller universe, never an error.
fn build_name_universe(session: Option<&UpgradeSession>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(s) = session {
        let by = s.secondary();
        names.extend(by.by_name.keys().map(|n| n.clone().into_inner()));
        names.extend(by.by_pkgbase.keys().map(|n| n.clone().into_inner()));
    }
    if let Ok(alpm) = alpm_db::open() {
        for db in alpm.syncdbs() {
            for pkg in db.pkgs() {
                names.push(pkg.name().to_owned());
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Production [`ShellEnv`]: the loaded session + stdout, bridging `upgrade` to
/// the existing loop.
struct RealEnv<'a> {
    cfg: &'a Config,
    devel: bool,
    session: Option<UpgradeSession>,
    names: Vec<String>,
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

    fn search(&mut self, terms: &[SearchTerm]) -> Result<Vec<ListItem>> {
        let regexes: Vec<regex::Regex> = terms
            .iter()
            .map(SearchTerm::compile)
            .collect::<std::result::Result<_, _>>()?;
        let color = ui::color_on();
        // Repo hits first (yay/paru "official repos on top"); they need no index.
        let mut rows: Vec<Row<'_>> = alpm_db::search_sync(terms)?
            .into_iter()
            .map(Row::Repo)
            .collect();
        if let Some(session) = &self.session {
            let mut aur = session.secondary().search(session.index(), &regexes);
            // Freshest commit first, pkgbase tie-break — same order as `-Ss`.
            aur.sort_by(|a, b| {
                b.commit_time_unix
                    .cmp(&a.commit_time_unix)
                    .then_with(|| a.pkgbase.cmp(&b.pkgbase))
            });
            rows.extend(aur.into_iter().map(Row::Aur));
        }
        Ok(rows
            .iter()
            .map(|r| ListItem {
                target: r.picked(),
                label: r.label(color),
            })
            .collect())
    }

    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
        let Some(session) = &self.session else {
            ui::warn("no AUR index; run `gaur -Sy` first");
            return Ok(());
        };
        // `info_targets` already warns about misses; the shell doesn't propagate
        // per-command exit codes, so the returned missing-list is discarded.
        index::info_targets(session.index(), session.secondary(), targets);
        Ok(())
    }

    fn names(&self) -> &[String] {
        &self.names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted [`ShellEnv`] capturing output + recording calls, with a
    /// pre-seeded search result and name universe, so dispatch is testable
    /// without a terminal, index, or alpm.
    #[derive(Default)]
    struct FakeEnv {
        lines: Vec<String>,
        upgrades: usize,
        search_result: Vec<ListItem>,
        info_calls: Vec<Vec<PkgTarget>>,
        names: Vec<String>,
    }

    impl ShellEnv for FakeEnv {
        fn print(&mut self, line: &str) {
            self.lines.push(line.to_owned());
        }
        fn upgrade(&mut self) -> Result<()> {
            self.upgrades += 1;
            Ok(())
        }
        fn search(&mut self, _terms: &[SearchTerm]) -> Result<Vec<ListItem>> {
            Ok(self.search_result.clone())
        }
        fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
            self.info_calls.push(targets.to_vec());
            Ok(())
        }
        fn names(&self) -> &[String] {
            &self.names
        }
    }

    fn li(label: &str, name: &str) -> ListItem {
        ListItem {
            target: PkgTarget::new(name),
            label: label.to_owned(),
        }
    }

    fn dispatch_one(input: &str) -> (Flow, FakeEnv) {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        let flow = dispatch(&command::parse(input), &mut state, &mut env);
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
        for verb in ["search", "info", "add", "upgrade", "apply", "quit"] {
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
    fn cart_verbs_are_acknowledged_stubs() {
        for input in ["add foo", "drop foo", "show", "apply", "clear"] {
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

    #[test]
    fn search_prints_numbered_list_and_remembers_it() {
        let mut env = FakeEnv {
            search_result: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        let flow = dispatch(&command::parse("search foo"), &mut state, &mut env);
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines
                .iter()
                .any(|l| l.starts_with("  1") && l.contains("aur/foo")),
            "row 1 should be numbered: {:?}",
            env.lines
        );
        assert!(
            env.lines
                .iter()
                .any(|l| l.contains("  2") && l.contains("extra/bar"))
        );
        assert_eq!(state.last_list.len(), 2, "the list should be remembered");
    }

    #[test]
    fn search_with_no_terms_prints_usage() {
        let (flow, env) = dispatch_one("search");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.iter().any(|l| l.contains("usage: search")));
    }

    #[test]
    fn info_by_number_resolves_against_the_last_list() {
        let mut env = FakeEnv::default();
        let mut state = State {
            last_list: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
        };
        dispatch(&command::parse("info 2"), &mut state, &mut env);
        assert_eq!(env.info_calls, vec![vec![PkgTarget::new("bar")]]);
    }

    #[test]
    fn info_by_name_passes_through() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        dispatch(&command::parse("info zlib"), &mut state, &mut env);
        assert_eq!(env.info_calls, vec![vec![PkgTarget::new("zlib")]]);
    }

    #[test]
    fn info_by_glob_resolves_against_names_universe() {
        let mut env = FakeEnv {
            names: vec!["python-bar".into(), "python-foo".into(), "ruby".into()],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        dispatch(&command::parse("info python-*"), &mut state, &mut env);
        assert_eq!(
            env.info_calls,
            vec![vec![
                PkgTarget::new("python-bar"),
                PkgTarget::new("python-foo")
            ]]
        );
    }

    #[test]
    fn info_out_of_range_number_reports_error_without_calling_show() {
        let mut env = FakeEnv::default();
        let mut state = State {
            last_list: vec![li("only 1-1", "only")],
        };
        dispatch(&command::parse("info 9"), &mut state, &mut env);
        assert!(env.info_calls.is_empty(), "must not show on a bad index");
        assert!(
            env.lines.iter().any(|l| l.contains("info:")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn info_with_no_args_prints_usage() {
        let (flow, env) = dispatch_one("info");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.iter().any(|l| l.contains("usage: info")));
    }
}
