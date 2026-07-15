//! Interactive shell (REPL) for the no-arg `aurox` invocation.
//!
//! A persistent prompt the user drives with word-commands (`search`, `add`,
//! `upgrade`, `apply`, …) against long-lived session state, replacing the
//! wizard-style `dialoguer` flows. See `docs/plans/shell-ui.md` for the full
//! design and phasing.
//!
//! **Phase 4 status:** the session is hoisted at start (the AUR index +
//! lookup maps via [`AurIndexData`], a sorted name universe for
//! globs/completion, and the sync-repo name set for coarse classification) and
//! is *reloaded* on `upgrade`. The cart is live: `add` / `drop` / `remove` /
//! `clear` stage a [`cart::Cart`]; `upgrade` refreshes + seeds the available
//! upgrades (repo approved / AUR needs-review); `review` / `approve` move AUR
//! items past the approval gate; `show` previews it; `apply` gates on
//! all-approved, then runs the partial `pacman -Syu` repo lane + the AUR
//! build/install + `pacman -R` removals, with the cost-overlay change-set
//! preview ([`upgrade`]). This replaced the old `upgrade_loop` driver +
//! dialoguer picker. `refresh [aur|pacman]` re-fetches the package data
//! (both halves, or one) and reloads the session without touching the cart.
//!
//! The [`ShellEnv`]/[`State::dispatch`] split keeps command handling
//! unit-testable with a scripted fake: the side-effecting I/O (classification,
//! the PKGBUILD diff, the refresh+recompute, the build) lives behind the trait
//! so the cart mutations and the approval gate are exercised without a
//! terminal, index, or `makepkg`.

use crate::build::DevelPolicy;
use crate::config::ConfigHandle;
use crate::error::{Error, Result};
use crate::index::{self, AurIndexData};
use crate::mirror;
use crate::names::{PkgBase, PkgTarget, RepoName, SearchTerm};
use crate::pacman::invoke::PkgUpgrade;
use crate::paths;
use crate::system;
use crate::ui;
use crate::units::ByteSize;
use cart::{ApplyOutcome, AurApproval, Cart, ReviewOutcome, StageClass};
use command::Command;
use complete::ShellHelper;
use env::{RealEnv, build_universe, cart_targets};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{ColorMode as RlColorMode, Config as RlConfig, Editor};
use std::rc::Rc;
use tracing::{debug, info, instrument};

pub mod cart;
pub mod command;
pub mod complete;
mod env;
mod help;
pub mod selector;
mod staging;
#[cfg(test)]
mod testenv;
pub mod upgrade;
mod verbs;

/// One row of a numbered list (search results or the cart), addressable by its
/// 1-based number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The thing `add` / `info` / … act on when this row is picked by number.
    pub target: PkgTarget,
    /// Preformatted display label (without the leading number).
    pub label: String,
    /// Repo bucket (`core`, `extra`, …, or `aur`) this row came from, so a
    /// repo-name selector (`add extra`) can filter the list. `None` for rows
    /// whose source isn't a repo (e.g. cart-derived selector lists).
    pub repo: Option<RepoName>,
}

/// Which numbered list a bare number (`3`, `2-4`) currently indexes.
///
/// The shell prints two kinds of numbered table — search results and the staged
/// transaction — and a number always means the row you last brought up. `search`
/// switches to [`View::Search`]; the verbs that bring the transaction to the
/// foreground (`show`, `upgrade`, `drop`, `keep`, `undo`) switch to
/// [`View::Cart`]. The list verbs (`add`, `remove`, `info`) read the active list
/// but leave the view alone, so working through a search list with a run of
/// `add`s keeps the numbers pointing at that list even though each `add`
/// reprints the cart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum View {
    /// Numbers index the most recent `search` result list.
    #[default]
    Search,
    /// Numbers index the staged cart rows (as `show` prints them).
    Cart,
}

/// How deep the `undo` stack goes — plenty for an interactive session, bounded
/// so a long-running shell can't grow it without limit.
const UNDO_DEPTH: usize = 64;

/// Mutable per-session shell state the dispatch core threads between commands.
#[derive(Default)]
pub struct State {
    /// The most recent `search` result list, indexed by number while the search
    /// view is active (see [`View`]).
    search_list: Vec<ListItem>,
    /// Which list bare numbers currently address — search results or the cart.
    view: View,
    /// The staged transaction `apply` runs.
    cart: Cart,
    /// Pre-change cart snapshots for `undo`, most-recent last. Each cart-changing
    /// command pushes the cart as it was before the change; `undo` pops the top.
    history: Vec<Cart>,
    /// Carts popped by `undo`, for `redo` to replay. Cleared by any fresh
    /// cart-changing command — a new edit forks a new branch, so the undone
    /// future is discarded (standard undo/redo semantics).
    redo: Vec<Cart>,
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
/// Behind a trait so the pure control flow ([`State::dispatch`]) is unit-testable
/// with a scripted fake. The cart mutations stay on [`State`]; this trait is the
/// I/O seam (search, classification, the PKGBUILD diff, the build+install).
pub trait ShellEnv {
    /// Emit one line of user-facing output.
    fn print(&mut self, line: &str);
    /// Refresh the mirror + index, reload the session (so `search`/`info` see
    /// fresh data too), and return the current upgrade candidates (repo ∪ AUR)
    /// for `upgrade` to seed into the cart.
    fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>>;
    /// Re-fetch the package data `scope` covers and reload the session (fresh
    /// data for `search`/`info`/classification/completion) **without** seeding
    /// the cart — `upgrade` is the stage-the-upgrades variant; `refresh` is
    /// just the re-fetch. The outcome says whether the AUR half actually
    /// refreshed or was skipped (never set up / AUR disabled / out of scope),
    /// so the dispatch core can word the result.
    fn refresh(&mut self, scope: mirror::RefreshScope) -> Result<mirror::RefreshOutcome>;
    /// Run a combined repo + AUR search; returns rows for the numbered list.
    fn search(&mut self, terms: &[SearchTerm]) -> Result<Vec<ListItem>>;
    /// Print `-Si`-style info for the already-resolved targets.
    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()>;
    /// Sorted universe of package targets, for glob resolution + completion.
    fn names(&self) -> &[PkgTarget];
    /// Coarse-classify a target for staging: a sync-repo package (with its
    /// concrete repo), an AUR package, or `None` when it's neither (a typo /
    /// unknown name). Only decides the approval policy and the `show` label —
    /// the real install routing is the resolver's call at `apply`.
    fn classify(&self, target: &PkgTarget) -> Option<StageClass>;
    /// Whether AUR items stage pre-approved — the effective `aur_approval`
    /// policy (see [`AurApproval::from_config`](cart::AurApproval::from_config)).
    fn aur_policy(&self) -> AurApproval;
    /// Where the AUR half stands this session — wording only (e.g. `add`'s
    /// unknown-name nudge); data flow stays uniform through the empty index.
    fn aur_state(&self) -> index::AurState;
    /// The pkgbase a staged AUR target resolves to, for the reviewed set fed
    /// into the build pipeline. `None` when it isn't a known AUR package.
    fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase>;
    /// Run the PKGBUILD review (diff-or-full) for one staged AUR target.
    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome>;
    /// Render the staged transaction table — the numbered install rows + the
    /// removal rows — colored, column-aligned, with a per-AUR-row "last
    /// modified" age. The header + approval summary stay in the pure dispatch
    /// core ([`State::show`]); this is the I/O-shaped presentation (color,
    /// width math, wall-clock age) that belongs behind the env seam.
    fn render_cart(&mut self, cart: &Cart);
    /// Run the staged transaction: resolve + preview + confirm + build/install +
    /// removals. Reads the cart; the dispatch core updates it from the outcome.
    fn apply(&mut self, cart: &Cart) -> Result<ApplyOutcome>;
    /// Measure aurox's on-disk state per category, for `system show`.
    /// Infallible: missing/unreadable paths report as zero.
    fn system_usage(&mut self) -> system::Report;
    /// `system prune`: delete the re-derivable caches (mirror, index, sync
    /// dbs, build trees) behind a y/N confirm. `Ok(None)` = user declined.
    /// Returns the bytes freed. The in-memory AUR data stays loaded — search
    /// and info keep working from it until a `refresh aur` re-fetches the
    /// mirror + index.
    fn system_prune(&mut self) -> Result<Option<ByteSize>>;
}

/// The pre-prompt banner: what this session covers. Pure so the wording is
/// testable. Runs *after* the first-launch question, so `NotSetUp` here means
/// the user chose "later" — one reminder line, not a re-pitch (the question
/// already spelled out the cost). Pacman-only mode gets a marker instead of a
/// nag: `aur = false` is a standing choice, not a missing step.
fn startup_lines(aur: index::AurState) -> Vec<&'static str> {
    match aur {
        index::AurState::Ready => {
            vec!["aurox shell — type `help` for commands, `quit` to leave"]
        }
        index::AurState::NotSetUp => vec![
            "aurox shell — type `help` for commands, `quit` to leave",
            "pacman-only this session — `refresh aur` syncs the AUR anytime",
        ],
        index::AurState::Disabled => {
            vec!["aurox shell (pacman-only) — type `help` for commands, `quit` to leave"]
        }
    }
}

/// The shell's first-launch question, asked while the AUR is enabled but was
/// never synced: sync now / pacman-only from now on / later.
///
/// Persistence is minimal by construction — "yes" persists as the mirror +
/// index artifact itself, "no" as an `aur = false` line written through
/// [`ConfigHandle::update`] (the one place aurox edits its own config, which
/// also flips the in-memory view so the rest of the session sees the
/// choice), "later" as nothing at all (asked again next launch).
fn first_launch_setup(mut config: ConfigHandle) -> Result<ConfigHandle> {
    if index::AurState::probe(config.cfg()) != index::AurState::NotSetUp {
        return Ok(config);
    }
    match ui::aur_setup_prompt().map_err(|e| Error::other(format!("setup prompt: {e}")))? {
        ui::AurSetupChoice::SyncNow => {
            // Consent was just given — ShellAurSync runs the bootstrap
            // without a second question.
            mirror::cmd_refresh(
                config.cfg(),
                mirror::RefreshReason::ShellAurSync,
                mirror::RefreshScope::Everything,
            )?;
        }
        ui::AurSetupChoice::PacmanOnly => {
            config.update(|c| c.aur = Some(false))?;
            ui::note(&format!(
                "pacman-only mode saved (`aur = false` in {}) — delete the line and `refresh aur` to opt back in",
                config.path().display()
            ));
        }
        ui::AurSetupChoice::Later => {}
    }
    Ok(config)
}

/// Run the interactive shell. Returns the desired process exit code.
///
/// `initial_search` seeds the session: when launched via the bare-positional
/// shortcut (`aurox <term>…`), dispatch passes the typed terms here and the shell
/// runs one `search` before the prompt loop — identical to starting the shell
/// and typing `search <term>…`. Empty for the plain no-arg `aurox` launch.
#[instrument(skip(config))]
pub fn run(config: &ConfigHandle, devel: DevelPolicy, initial_search: &[SearchTerm]) -> Result<u8> {
    info!(devel = ?devel, terms = initial_search.len(), "shell session start");
    // First-launch question (no-op unless the AUR is enabled-but-unsynced).
    // Owns a local handle so a "pacman-only" answer takes effect immediately.
    let config = first_launch_setup(config.clone())?;
    let cfg = config.cfg();
    // Once per session: load the AUR index (+ lookup maps) and the name
    // universe. Not repeated per command; `refresh` (later phase) re-fetches.
    // The AUR data loads empty (not absent) when the AUR isn't in play.
    let aur_state = index::AurState::probe(cfg);
    let aur_data = AurIndexData::load(cfg)?;
    let caches = build_universe(&aur_data);
    debug!(
        names = caches.universe.len(),
        sync = caches.sync.len(),
        aur = ?aur_state,
        "shell session loaded"
    );
    let mut env = RealEnv {
        cfg,
        devel,
        aur_data,
        aur_state,
        caches,
        view: None,
    };
    let mut state = State::default();

    for line in startup_lines(aur_state) {
        env.print(line);
    }

    // Seed the session with the launch-time search (`aurox <term>…`): run it once
    // up front so the numbered result list is on screen before the first prompt,
    // exactly as if the user had typed `search <term>…`.
    if !initial_search.is_empty() {
        state.dispatch(&Command::Search(initial_search.to_vec()), &mut env);
    }

    let helper = ShellHelper::new(Rc::clone(&env.caches.universe));
    // Follow the session's colour mode so `--color never` also stops rustyline
    // from dimming the history hint (it skips `highlight_hint` when Disabled).
    let rl_config = RlConfig::builder()
        .color_mode(match cfg.color_mode() {
            ui::ColorMode::Always => RlColorMode::Forced,
            ui::ColorMode::Never => RlColorMode::Disabled,
            ui::ColorMode::Auto => RlColorMode::Enabled,
        })
        .build();
    let mut rl: Editor<ShellHelper, DefaultHistory> = Editor::with_config(rl_config)
        .map_err(|e| Error::other(format!("shell: init line editor: {e}")))?;
    rl.set_helper(Some(helper));
    let history = paths::shell_history_path();
    // A missing history file on first run is expected, not an error.
    rl.load_history(&history).ok();

    let code = loop {
        match rl.readline("aurox> ") {
            Ok(line) => {
                if !line.trim().is_empty() {
                    // Best-effort: a full history ring shouldn't abort input.
                    rl.add_history_entry(line.as_str()).ok();
                }
                let flow = state.dispatch(&command::parse(&line), &mut env);
                // Refresh Tab's view for the next line: the just-mutated cart,
                // and the universe (a cheap `Rc` clone — only `upgrade`/`refresh`
                // actually swaps it). Sharing the same sources the selector
                // resolver uses keeps "what Tab offers" == "what the verb accepts".
                if let Some(helper) = rl.helper_mut() {
                    helper.sync(Rc::clone(&env.caches.universe), cart_targets(&state));
                }
                if let Flow::Exit(code) = flow {
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::assert_contains;

    /// The pre-prompt banner: a ready session gets the one-liner, a "later"
    /// answer gets one reminder line (the launch question already pitched
    /// the cost), and pacman-only mode is marked instead of nagged.
    #[test]
    fn startup_banner_variants() {
        let ready = startup_lines(index::AurState::Ready);
        assert_eq!(ready.len(), 1, "ready session banners one line: {ready:?}");

        let later = startup_lines(index::AurState::NotSetUp);
        assert_eq!(
            later.len(),
            2,
            "one reminder line, not a re-pitch: {later:?}"
        );
        assert_contains!(later[1], "`refresh aur`");

        let pacman_only = startup_lines(index::AurState::Disabled);
        assert_eq!(
            pacman_only.len(),
            1,
            "pacman-only mode must not nag: {pacman_only:?}"
        );
        assert_contains!(pacman_only[0], "(pacman-only)");
    }
}
