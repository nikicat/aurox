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
use cart::{ApplyOutcome, AurApproval, Cart, CartItem, ReviewOutcome, StageClass, StageResult};
use command::{Command, SystemAction};
use complete::ShellHelper;
use env::{RealEnv, build_universe, cart_targets};
use help::{HELP_TEXT, help_topic};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{ColorMode as RlColorMode, Config as RlConfig, Editor};
use std::collections::HashSet;
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

/// Word one `refresh` outcome — the AUR half only. The repo-database half
/// reports for itself from inside [`mirror::cmd_refresh`] (refreshed / up to
/// date / failed) and doesn't run at all when `check_repo_updates` is off,
/// so any claim about it here would double-report at best and lie at worst.
/// `None` when there is nothing to say about the AUR half — `refresh pacman`
/// scoped it out on purpose, so the repo half's own report is the whole story.
const fn refresh_message(outcome: mirror::RefreshOutcome) -> Option<&'static str> {
    match outcome {
        mirror::RefreshOutcome::Refreshed => Some("mirror + index refreshed"),
        mirror::RefreshOutcome::AurSkipped(mirror::SkipCause::NotSetUp) => {
            Some("AUR not synced — `refresh aur` runs the one-time setup")
        }
        mirror::RefreshOutcome::AurSkipped(
            mirror::SkipCause::Declined | mirror::SkipCause::NonInteractive,
        ) => Some("AUR setup skipped — run `refresh aur` when ready"),
        mirror::RefreshOutcome::AurSkipped(mirror::SkipCause::Disabled) => {
            Some("AUR refresh skipped (aur = false in config.toml)")
        }
        mirror::RefreshOutcome::AurSkipped(mirror::SkipCause::NotRequested) => None,
    }
}

/// `refresh [aur|pacman]` — re-fetch what the scope covers and reload the
/// session; the cart is left untouched (`upgrade` is the seed-the-cart
/// variant). A free function like [`system_dispatch`]: it reads no session
/// state, only the env seam. `None` is an unrecognized scope word — usage
/// line, never a silently-widened full refresh.
fn refresh_dispatch<E: ShellEnv>(scope: Option<mirror::RefreshScope>, env: &mut E) {
    match scope {
        None => env.print("usage: refresh [aur|pacman] — see `help refresh`"),
        Some(scope) => match env.refresh(scope) {
            Ok(outcome) => {
                if let Some(msg) = refresh_message(outcome) {
                    env.print(msg);
                }
            }
            Err(e) => env.print(&format!("refresh: {e}")),
        },
    }
}

/// `system <show|prune>` — the maintenance group. A free function rather than
/// a [`State`] method: it reads no session state (no cart, no lists), only the
/// env seam.
fn system_dispatch<E: ShellEnv>(action: Option<SystemAction>, env: &mut E) {
    match action {
        None => env.print("usage: system <show|prune> — see `help system`"),
        Some(SystemAction::Show) => {
            let report = env.system_usage();
            print_system_report(&report, env);
        }
        Some(SystemAction::Prune) => match env.system_prune() {
            Ok(Some(freed)) => env.print(&format!(
                "caches pruned — {freed} freed; `refresh aur` re-fetches the mirror + index"
            )),
            Ok(None) => env.print("prune cancelled — nothing removed"),
            Err(e) => env.print(&format!("prune: {e}")),
        },
    }
}

/// Render the `system show` table through `env`: one aligned row per state
/// category (size + what it holds, cache rows tagged) and a total line saying
/// what `system prune` would free.
// TODO: consolidate the table-formatting code — this hand-rolled column
// layout, the width math inside `ui::search_table` / `ui::transaction_table`,
// and the HELP_TEXT column convention each re-implement aligned columns;
// `ui::Table` only collects rendered lines. One shared column-layout helper
// should own padding/alignment so the conventions can't drift per call site.
fn print_system_report<E: ShellEnv>(report: &system::Report, env: &mut E) {
    env.print(&format!("state under {}:", report.root.display()));
    for row in &report.rows {
        let tag = if row.kind.prunable() { "  [cache]" } else { "" };
        env.print(&format!(
            "  {:<8} {:>10}  {}{tag}",
            row.kind.label(),
            row.size,
            row.kind.description(),
        ));
    }
    env.print(&format!(
        "  {:<8} {:>10}  `system prune` frees the [cache] rows ({})",
        "total",
        report.total(),
        report.prunable_total(),
    ));
}

/// Pure command dispatch: map a parsed [`Command`] to side effects + control
/// flow.
///
/// Side effects go through `env`/`self`; dispatch does no I/O of its own, so the
/// command surface and exit conditions are testable without a terminal. Each
/// argument-bearing verb is a method on [`State`] below.
impl State {
    pub fn dispatch<E: ShellEnv>(&mut self, cmd: &Command, env: &mut E) -> Flow {
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
            Command::Help(topic) => {
                match topic {
                    None => env.print(HELP_TEXT),
                    Some(t) => env.print(&help_topic(t)),
                }
                Flow::Continue
            }
            Command::Search(terms) => {
                self.search(terms, env);
                Flow::Continue
            }
            Command::Info(args) => {
                self.info(args, env);
                Flow::Continue
            }
            Command::Upgrade(args) => {
                self.upgrade(args, env);
                Flow::Continue
            }
            Command::Add(args) => {
                self.add(args, env);
                Flow::Continue
            }
            Command::Drop(args) => {
                self.discard(args, env);
                Flow::Continue
            }
            Command::Keep(args) => {
                self.keep(args, env);
                Flow::Continue
            }
            Command::Remove(args) => {
                self.remove(args, env);
                Flow::Continue
            }
            Command::Approve(args) => {
                self.approve(args, env);
                Flow::Continue
            }
            Command::Review(args) => {
                self.review(args, env);
                Flow::Continue
            }
            Command::Show => {
                // `show` brings the transaction to the foreground, so numbers now
                // address its rows.
                self.view = View::Cart;
                self.show(env);
                Flow::Continue
            }
            Command::Apply => {
                self.apply(env);
                Flow::Continue
            }
            Command::Undo => {
                self.undo(env);
                Flow::Continue
            }
            Command::Redo => {
                self.redo(env);
                Flow::Continue
            }
            Command::Clear => {
                if self.cart.is_empty() {
                    env.print("cart is already empty");
                } else {
                    self.push_undo(self.cart.clone());
                    self.cart.clear();
                    env.print("cart cleared — `undo` to restore");
                }
                Flow::Continue
            }
            Command::Refresh(scope) => {
                refresh_dispatch(*scope, env);
                Flow::Continue
            }
            Command::System(action) => {
                system_dispatch(*action, env);
                Flow::Continue
            }
        }
    }

    /// `search <terms…>`: run the query, print a numbered list, remember it.
    fn search<E: ShellEnv>(&mut self, terms: &[SearchTerm], env: &mut E) {
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
                    // `items` is best-first (row 1 = best). Print it worst-first
                    // so the strongest matches land at the bottom, next to the
                    // prompt the shell scrolls to — and the low, easy-to-type
                    // numbers are the good ones. The numbers still key the
                    // best-first `search_list`, so `add 1` is always the top match
                    // regardless of print direction.
                    for (i, item) in items.iter().enumerate().rev() {
                        env.print(&format!("{:>3}  {}", i + 1, item.label));
                    }
                }
                // Replace the current list even when empty, so a stale list
                // can't be addressed by number after a fruitless search, and
                // make the search results the active numbered view.
                self.search_list = items;
                self.view = View::Search;
            }
            Err(e) => env.print(&format!("search: {e}")),
        }
    }

    /// `info <sel…>`: resolve the selectors and show details. Reads the current
    /// list but doesn't mutate session state.
    fn info<E: ShellEnv>(&self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: info <pkg|number|range|glob>…");
            return;
        }
        let targets = match self.resolve_against_list(args, env) {
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

    /// `upgrade [sel…]`: refresh + recompute the available upgrades and seed
    /// them into the cart (repo → approved, AUR → needs-review per config). With
    /// `sel…`, seed only the matching subset (numbers index the freshly computed
    /// list; names/globs match candidate names). Then `show`s the cart.
    fn upgrade<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        let candidates = match env.upgrade() {
            Ok(c) => c,
            Err(e) => {
                env.print(&format!("upgrade: {e}"));
                return;
            }
        };
        if candidates.is_empty() {
            env.print("nothing to upgrade");
            return;
        }
        let to_seed = if args.is_empty() {
            candidates
        } else {
            match select_from_candidates(args, &candidates) {
                Ok(v) => v,
                Err(e) => {
                    env.print(&format!("upgrade: {e}"));
                    return;
                }
            }
        };
        let policy = env.aur_policy();
        let before = self.cart.clone();
        let mut staged = 0;
        for u in to_seed {
            if self.cart.add(CartItem::from_upgrade(u, policy)) == StageResult::Staged {
                staged += 1;
            }
        }
        if staged > 0 {
            self.push_undo(before);
        }
        // The seeded transaction is now the foreground list.
        self.view = View::Cart;
        env.print(&format!("{staged} upgrade(s) staged"));
        self.show(env);
    }

    /// `show`: render the staged transaction — a header, the install/removal
    /// table (delegated to the env for color + alignment + age), and whether
    /// `apply` is ready.
    ///
    /// The header and the approval summary are deterministic and stay here in
    /// the pure core (so they're unit-testable via the fake env); the table body
    /// — color, column widths, per-AUR-row age — is I/O-shaped presentation and
    /// goes through [`ShellEnv::render_cart`].
    fn show<E: ShellEnv>(&self, env: &mut E) {
        let cart = &self.cart;
        if cart.is_empty() {
            env.print("cart is empty — `add <pkg>` to stage an install");
            return;
        }
        env.print(&format!(
            "transaction — {} to install, {} to remove",
            cart.items().len(),
            cart.removals().len()
        ));
        env.render_cart(cart);
        let pending = cart.pending_review().len();
        if pending == 0 {
            env.print("all approved — run `apply`");
        } else {
            env.print(&format!(
                "{pending} package(s) need review — run `review <sel>` or `approve <sel>`"
            ));
        }
    }

    /// `apply`: gate on every staged item being approved, then run the
    /// transaction. A clean run clears the applied rows; a declined one keeps
    /// the cart; a failed one keeps it intact so the user can `drop` the
    /// offender and retry.
    fn apply<E: ShellEnv>(&mut self, env: &mut E) {
        if self.cart.is_empty() {
            env.print("cart is empty — nothing to apply");
            return;
        }
        let pending = self.cart.pending_review();
        if !pending.is_empty() {
            let names: Vec<&str> = pending.iter().map(|i| i.spec()).collect();
            env.print(&format!(
                "needs review: {} — run `review <sel>` or `approve <sel>`",
                names.join(", ")
            ));
            return;
        }
        match env.apply(&self.cart) {
            Ok(ApplyOutcome::Declined) => env.print("apply cancelled — cart kept"),
            Ok(ApplyOutcome::Succeeded) => {
                self.cart.clear_applied();
                // The transaction ran: the cart is a new epoch, so pre-apply
                // undo snapshots (which would re-stage now-installed packages)
                // no longer make sense.
                self.clear_undo_history();
                env.print("done");
            }
            Ok(ApplyOutcome::Failed { installed }) => {
                // Drop the rows that actually landed so a retry doesn't reinstall
                // them; keep the offenders (and any staged removals, which don't
                // run once a build fails) staged for `drop`/fix + `apply` again.
                let landed = installed.len();
                for t in &installed {
                    self.cart.unstage(t);
                }
                // A run happened — old undo snapshots reference a pre-apply world.
                self.clear_undo_history();
                if landed == 0 {
                    env.print("apply failed — nothing installed; cart kept for retry");
                } else {
                    env.print(&format!(
                        "apply partly failed — {landed} installed (dropped), \
                         {} still staged; fix and `apply` again",
                        self.cart.items().len()
                    ));
                }
                // Reprint what's left so the failures are on screen to act on.
                self.view = View::Cart;
                self.show(env);
            }
            Err(e) => env.print(&format!("apply: {e}")),
        }
    }

    /// Forget the `undo`/`redo` stacks — after a transaction runs, the snapshots
    /// describe a world that no longer exists.
    fn clear_undo_history(&mut self) {
        self.history.clear();
        self.redo.clear();
    }

    /// Resolve selector `args` for a cart verb (`drop`, `keep`, `approve`,
    /// `review`): a repo name (`aur`, `core`, …) selects every staged row from
    /// that repo, and names/globs match staged specs — both scoped to what's
    /// staged, since a cart verb acts on the cart regardless of which list is up.
    /// Numbers, though, index the *active* list (see [`View`]), so a bare `3`
    /// means the same row it would for any other verb — the one you last saw.
    fn resolve_against_cart(&self, args: &[String]) -> std::result::Result<Vec<PkgTarget>, String> {
        let rows: Vec<RepoRow> = self
            .cart
            .items()
            .iter()
            .map(|it| RepoRow {
                target: PkgTarget::new(it.spec()),
                repo: Some(it.repo_label()),
            })
            .collect();
        let args = expand_repo_tokens(args, &rows);
        let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
        selector::resolve(&args, &self.active_list(), &universe)
    }

    /// Resolve selector `args` for a list verb (`add`, `info`, `remove`): a repo
    /// name selects every row from that repo in the active list, numbers/ranges
    /// index the active list, and names/globs resolve against the full name
    /// universe (so you can `add` anything installable, not just what's shown).
    fn resolve_against_list<E: ShellEnv>(
        &self,
        args: &[String],
        env: &E,
    ) -> std::result::Result<Vec<PkgTarget>, String> {
        let active = self.active_list();
        let rows: Vec<RepoRow> = active
            .iter()
            .map(|it| RepoRow {
                target: it.target.clone(),
                repo: it.repo.clone(),
            })
            .collect();
        let args = expand_repo_tokens(args, &rows);
        selector::resolve(&args, &active, env.names())
    }

    /// The staged cart as a numbered list — the same rows, in the same order,
    /// that `show` prints — so a number resolves to the row the user sees. Built
    /// live from the cart, so it can never lag a staging change.
    fn cart_as_list(&self) -> Vec<ListItem> {
        self.cart
            .items()
            .iter()
            .map(|it| ListItem {
                target: PkgTarget::new(it.spec()),
                label: String::new(),
                repo: Some(it.repo_label()),
            })
            .collect()
    }

    /// The list bare numbers currently index: the search results while the search
    /// view is up, else the staged cart (see [`View`]).
    ///
    /// The search view falls back to the cart when there's no search list to
    /// address — a fresh session (never searched) or a fruitless search — so a
    /// number always resolves against whatever numbered table is actually on
    /// screen, which after an `add`/`drop`/… is the cart.
    fn active_list(&self) -> Vec<ListItem> {
        match self.view {
            View::Search if !self.search_list.is_empty() => self.search_list.clone(),
            _ => self.cart_as_list(),
        }
    }

    /// Snapshot the pre-change cart onto the `undo` stack (bounded) and discard
    /// any redo branch. Call with the cart as it was *before* a cart-changing
    /// command mutates it — only when the command actually changed something, so
    /// a no-op never consumes an undo step.
    fn push_undo(&mut self, before: Cart) {
        self.history.push(before);
        if self.history.len() > UNDO_DEPTH {
            self.history.remove(0);
        }
        self.redo.clear();
    }

    /// `undo`: revert the last cart-changing command, restoring the cart to how
    /// it was before it ran. The reverted-from cart moves onto the redo stack.
    fn undo<E: ShellEnv>(&mut self, env: &mut E) {
        match self.history.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut self.cart, prev));
                self.view = View::Cart;
                env.print("undone — `redo` to reapply");
                self.show(env);
            }
            None => env.print("nothing to undo"),
        }
    }

    /// `redo`: reapply the most recently undone change. The inverse of `undo`;
    /// available only until a fresh cart-changing command clears the redo branch.
    fn redo<E: ShellEnv>(&mut self, env: &mut E) {
        match self.redo.pop() {
            Some(next) => {
                self.history.push(std::mem::replace(&mut self.cart, next));
                self.view = View::Cart;
                env.print("redone");
                self.show(env);
            }
            None => env.print("nothing to redo"),
        }
    }
}

/// One `(target, repo)` pair fed to [`expand_repo_tokens`] — the minimal view of
/// a cart row or list row a repo-name selector needs.
struct RepoRow {
    target: PkgTarget,
    repo: Option<RepoName>,
}

/// Rewrite repo-name tokens (`aur`, `core`, `extra`, …) into the targets of the
/// rows whose repo matches, so `drop aur` / `add extra` act on a whole repo.
///
/// A token that matches no row's repo is passed through unchanged for the
/// number/range/name/glob selector to handle — so a real package that happens
/// to share a repo's name still resolves normally when nothing in the current
/// scope is from that repo. Matching is case-insensitive. The expansion emits
/// selector tokens (the matched targets' names) so the one resolution path in
/// [`selector::resolve`] still does the indexing, dedup, and ordering.
fn expand_repo_tokens(args: &[String], rows: &[RepoRow]) -> Vec<String> {
    args.iter()
        .flat_map(|a| {
            let matched: Vec<String> = rows
                .iter()
                .filter(|r| {
                    r.repo
                        .as_ref()
                        .is_some_and(|repo| repo.as_str().eq_ignore_ascii_case(a))
                })
                .map(|r| r.target.as_str().to_owned())
                .collect();
            if matched.is_empty() {
                vec![a.clone()]
            } else {
                matched
            }
        })
        .collect()
}

/// Filter `candidates` to those a selector matches: a repo name (`aur`, `core`,
/// …) selects every candidate from that repo; numbers index the candidate list;
/// names/globs match candidate names. Reuses the selector core (the same one
/// `add`/`info`/cart verbs use), so `upgrade glibc python-*` and `upgrade aur`
/// work the same.
fn select_from_candidates(
    args: &[String],
    candidates: &[PkgUpgrade],
) -> std::result::Result<Vec<PkgUpgrade>, String> {
    let rows: Vec<RepoRow> = candidates
        .iter()
        .map(|u| RepoRow {
            target: PkgTarget::new(u.name.as_str()),
            repo: Some(u.repo.clone()),
        })
        .collect();
    let args = expand_repo_tokens(args, &rows);
    let list: Vec<ListItem> = rows
        .iter()
        .map(|r| ListItem {
            target: r.target.clone(),
            label: String::new(),
            repo: r.repo.clone(),
        })
        .collect();
    let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
    let picked = selector::resolve(&args, &list, &universe)?;
    let names: HashSet<&str> = picked.iter().map(PkgTarget::as_str).collect();
    Ok(candidates
        .iter()
        .filter(|u| names.contains(u.name.as_str()))
        .cloned()
        .collect())
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

    use super::cart::Source;
    use super::testenv::{FakeEnv, cart_specs, dispatch_one, env_with, li, li_repo, up};
    use crate::{assert_contains, assert_regex};

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
                .any(|l| l.contains("unknown command") && l.contains("frobnicate")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn upgrade_seeds_the_cart_repo_approved_aur_needs_review() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env);
        assert_eq!(env.upgrades.count(), 1, "upgrade recomputes once");
        assert_eq!(state.cart.items().len(), 2, "both candidates staged");
        // Repo upgrade auto-approves; AUR upgrade needs review.
        assert_eq!(state.cart.pending_review().len(), 1);
        assert_eq!(state.cart.pending_review()[0].spec(), "yay-bin");
    }

    #[test]
    fn upgrade_with_selector_seeds_only_the_subset() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade yay-bin"), &mut env);
        let specs: Vec<&str> = state.cart.items().iter().map(CartItem::spec).collect();
        assert_eq!(specs, vec!["yay-bin"]);
    }

    #[test]
    fn upgrade_with_nothing_to_do_stages_nothing() {
        let (flow, env) = dispatch_one("upgrade");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("nothing to upgrade"));
    }

    #[test]
    fn refresh_reloads_without_seeding_or_touching_the_cart() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("refresh"), &mut env);
        assert_eq!(env.refreshes.count(), 1, "refresh re-fetches once");
        assert_eq!(
            env.refresh_scopes,
            vec![mirror::RefreshScope::Everything],
            "a bare refresh covers everything"
        );
        assert_eq!(
            env.upgrades.count(),
            0,
            "refresh is not an upgrade recompute"
        );
        assert_eq!(
            state.cart.items().len(),
            1,
            "refresh leaves the cart intact"
        );
        assert!(env.lines.contains("refreshed"));
    }

    /// `refresh aur` / `refresh pacman` narrow the scope; the words come from
    /// the one table the parser and completer share.
    #[test]
    fn refresh_scope_words_reach_the_env() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("refresh aur"), &mut env);
        state.dispatch(&command::parse("refresh pacman"), &mut env);
        assert_eq!(
            env.refresh_scopes,
            vec![mirror::RefreshScope::Aur, mirror::RefreshScope::Pacman]
        );
    }

    /// A typo'd scope prints usage and never reaches the env — it must not
    /// silently widen into a full refresh.
    #[test]
    fn refresh_with_unknown_scope_prints_usage_and_does_nothing() {
        let mut env = FakeEnv::default();
        State::default().dispatch(&command::parse("refresh pacmna"), &mut env);
        assert_eq!(env.refreshes.count(), 0);
        assert!(
            env.lines
                .any(|l| l.starts_with("usage: refresh [aur|pacman]")),
            "{:?}",
            env.lines
        );
    }

    /// `refresh pacman` scoped the AUR half out on purpose: the repo half
    /// reports for itself inside `cmd_refresh`, so the dispatch core adds no
    /// line of its own (an "AUR skipped" note would be noise).
    #[test]
    fn refresh_pacman_scope_says_nothing_about_the_aur() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::NotRequested,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh pacman"), &mut env);
        assert_eq!(env.refreshes.count(), 1);
        assert!(env.lines.is_empty(), "{:?}", env.lines);
    }

    /// A bare `refresh` in a never-synced session stays pacman-only and
    /// points at `refresh aur` — it must never read as a full refresh.
    #[test]
    fn refresh_not_set_up_words_the_skip_with_the_aur_hint() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::NotSetUp,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh"), &mut env);
        assert!(
            env.lines
                .any(|l| l.contains("AUR not synced") && l.contains("`refresh aur`")),
            "{:?}",
            env.lines
        );
        assert!(!env.lines.contains("mirror + index refreshed"));
    }

    /// A declined bootstrap is worded as a skip (with the retry hint), not as
    /// a full "mirror + index refreshed".
    #[test]
    fn refresh_decline_words_the_skip() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::Declined,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh"), &mut env);
        assert!(env.lines.contains("AUR setup skipped"));
        assert!(!env.lines.contains("mirror + index refreshed"));
    }

    /// Pacman-only mode: `refresh` words the AUR skip and claims nothing
    /// about the repo half — that half reports for itself from inside
    /// `cmd_refresh`, and doesn't run at all with `check_repo_updates` off.
    #[test]
    fn refresh_disabled_words_the_skip_without_repo_claims() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::Disabled,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh"), &mut env);
        assert!(
            env.lines
                .contains("AUR refresh skipped (aur = false in config.toml)")
        );
        assert!(!env.lines.contains("refreshed"));
    }

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

    #[test]
    fn system_without_action_prints_usage_and_never_prunes() {
        // The safety the two-word group exists for: neither a bare `system`
        // nor a typo'd action may fall through to the prune.
        for line in ["system", "system wat"] {
            let mut env = FakeEnv::default();
            State::default().dispatch(&command::parse(line), &mut env);
            assert!(
                env.lines.contains("usage: system"),
                "`{line}`: {:?}",
                env.lines
            );
            assert_eq!(env.prune_calls.count(), 0, "`{line}` must not prune");
        }
    }

    #[test]
    fn system_show_renders_rows_with_cache_tags_and_the_totals() {
        let mut env = FakeEnv {
            usage_rows: vec![
                system::Usage {
                    kind: system::StateKind::Mirror,
                    size: ByteSize::new(2 * 1024 * 1024 * 1024),
                },
                system::Usage {
                    kind: system::StateKind::Metrics,
                    size: ByteSize::new(1024),
                },
            ],
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("system show"), &mut env);
        assert!(env.lines.contains("state under /state"), "{:?}", env.lines);
        // One anchored regex per rendered row: label, aligned size, description,
        // and the [cache] tag only on the prunable row.
        assert_regex!(
            env.lines.joined(),
            r"(?m)^  mirror\s+2\.00 GiB\s+AUR git mirror\s+\[cache\]$"
        );
        assert_regex!(
            env.lines.joined(),
            r"(?m)^  metrics\s+1\.00 KiB\s+build-time history$"
        );
        // The total sums both rows; the prunable half quotes only the mirror.
        assert_regex!(
            env.lines.joined(),
            r"(?m)^  total\s+2\.00 GiB\s+`system prune` frees the \[cache\] rows \(2\.00 GiB\)$"
        );
        assert_eq!(env.prune_calls.count(), 0, "show must not prune");
    }

    #[test]
    fn system_prune_reports_the_freed_bytes() {
        let mut env = FakeEnv {
            prune_outcome: Some(ByteSize::new(3 * 1024 * 1024)),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("system prune"), &mut env);
        assert_eq!(env.prune_calls.count(), 1);
        assert!(
            env.lines
                .any(|l| l.contains("3.00 MiB freed") && l.contains("`refresh aur`")),
            "{:?}",
            env.lines
        );
    }

    #[test]
    fn system_prune_declined_reports_cancellation() {
        // `prune_outcome: None` scripts the user answering N at the confirm.
        let mut env = FakeEnv::default();
        State::default().dispatch(&command::parse("system prune"), &mut env);
        assert_eq!(env.prune_calls.count(), 1, "the env owns the prompt");
        assert!(env.lines.contains("cancelled"), "{:?}", env.lines);
    }

    #[test]
    fn clear_empties_the_cart() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("clear"), &mut env);
        assert!(state.cart.is_empty());
    }

    #[test]
    fn apply_gate_blocks_while_items_need_review() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(
            env.apply_calls.count(),
            0,
            "apply must not run while pending"
        );
        assert!(env.lines.contains("needs review"));
    }

    #[test]
    fn apply_runs_when_all_approved_and_clears_on_success() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(env.apply_calls.count(), 1);
        assert!(state.cart.is_empty(), "a clean apply clears the cart");
        assert!(env.lines.any(|l| l == "done"));
    }

    #[test]
    fn apply_declined_keeps_the_cart() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        env.apply_outcome = Some(ApplyOutcome::Declined);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(state.cart.items().len(), 1, "declined apply keeps the cart");
    }

    #[test]
    fn apply_total_failure_keeps_the_whole_cart_for_retry() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        // Nothing landed → empty `installed` → the whole cart stays staged.
        env.apply_outcome = Some(ApplyOutcome::Failed {
            installed: Vec::new(),
        });
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(state.cart.items().len(), 1, "failed apply keeps the cart");
        assert!(env.lines.contains("cart kept for retry"));
    }

    #[test]
    fn apply_partial_failure_drops_landed_rows_and_keeps_the_failures() {
        // Regression: `upgrade` stages 4 AUR packages, 2 build + install and 2
        // fail. The cart must keep only the 2 that failed — not show all 4.
        let mut env = env_with(&[
            ("a", Source::Aur),
            ("b", Source::Aur),
            ("c", Source::Aur),
            ("d", Source::Aur),
        ]);
        // `a` and `b` landed; `c` and `d` didn't.
        env.apply_outcome = Some(ApplyOutcome::Failed {
            installed: vec![PkgTarget::new("a"), PkgTarget::new("b")],
        });
        let mut state = State::default();
        state.dispatch(&command::parse("add a b c d"), &mut env);
        state.dispatch(&command::parse("approve *"), &mut env); // clear the gate
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["c", "d"],
            "only the failed packages stay staged"
        );
        assert!(env.lines.contains("apply partly failed"));
    }

    #[test]
    fn show_reports_pending_then_ready() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("show"), &mut env);
        assert!(env.lines.contains("need review"));
        state.dispatch(&command::parse("approve yay-bin"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("show"), &mut env);
        assert!(env.lines.contains("all approved"));
    }

    #[test]
    fn syntax_error_is_reported_not_fatal() {
        let (flow, env) = dispatch_one("add \"unterminated");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("syntax error"), "got: {:?}", env.lines);
    }

    #[test]
    fn search_prints_numbered_list_and_remembers_it() {
        let mut env = FakeEnv {
            search_result: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        let flow = state.dispatch(&command::parse("search foo"), &mut env);
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines
                .any(|l| l.starts_with("  1") && l.contains("aur/foo")),
            "row 1 should be numbered: {:?}",
            env.lines
        );
        assert!(
            env.lines
                .any(|l| l.contains("  2") && l.contains("extra/bar"))
        );
        assert_eq!(state.search_list.len(), 2, "the list should be remembered");
    }

    #[test]
    fn search_with_no_terms_prints_usage() {
        let (flow, env) = dispatch_one("search");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("usage: search"));
    }

    #[test]
    fn info_by_number_resolves_against_the_search_list() {
        let mut env = FakeEnv::default();
        let mut state = State {
            search_list: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
            ..State::default()
        };
        state.dispatch(&command::parse("info 2"), &mut env);
        assert_eq!(env.info_calls, vec![vec![PkgTarget::new("bar")]]);
    }

    #[test]
    fn info_by_name_passes_through() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("info zlib"), &mut env);
        assert_eq!(env.info_calls, vec![vec![PkgTarget::new("zlib")]]);
    }

    #[test]
    fn info_by_glob_resolves_against_names_universe() {
        let mut env = FakeEnv {
            names: vec!["python-bar".into(), "python-foo".into(), "ruby".into()],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("info python-*"), &mut env);
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
            search_list: vec![li("only 1-1", "only")],
            ..State::default()
        };
        state.dispatch(&command::parse("info 9"), &mut env);
        assert!(env.info_calls.is_empty(), "must not show on a bad index");
        assert!(env.lines.contains("info:"), "got: {:?}", env.lines);
    }

    #[test]
    fn info_with_no_args_prints_usage() {
        let (flow, env) = dispatch_one("info");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("usage: info"));
    }

    #[test]
    fn upgrade_by_repo_filter_seeds_only_that_repo() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade aur"), &mut env);
        assert_eq!(cart_specs(&state), vec!["yay-bin"]);
    }

    // --- unified numbering: a bare number follows the last-shown list ---

    #[test]
    fn add_by_number_keeps_pointing_at_the_search_list_across_adds() {
        // Working through a search list: each `add` reprints the cart but must
        // not yank the numbering onto it, so `add 1` then `add 3` both index the
        // search rows (the classic "search, then add a few" flow).
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur), ("c", Source::Aur)]);
        env.search_result = vec![
            li_repo("aur", "a"),
            li_repo("aur", "b"),
            li_repo("aur", "c"),
        ];
        let mut state = State::default();
        state.dispatch(&command::parse("search x"), &mut env);
        state.dispatch(&command::parse("add 1"), &mut env);
        state.dispatch(&command::parse("add 3"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["a", "c"],
            "1 and 3 index the search list, not the reprinted cart"
        );
    }

    #[test]
    fn number_indexes_the_cart_when_no_search_was_run() {
        // Fresh session, staged straight into the cart (no `search`): a bare
        // number must still resolve against the cart on screen, not error with
        // "no numbered list is up".
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env); // cart: [bar, foo]
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "`drop 1` hits the shown cart"
        );
    }

    #[test]
    fn drop_by_number_indexes_the_shown_cart() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // cart: [bar, foo]
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "`drop 1` drops shown row 1"
        );
    }

    #[test]
    fn show_switches_numbering_from_the_search_list_to_the_cart() {
        let mut env = env_with(&[("staged", Source::Aur)]);
        env.search_result = vec![li_repo("aur", "searched")];
        let mut state = State::default();
        state.dispatch(&command::parse("add staged"), &mut env); // cart = [staged]
        state.dispatch(&command::parse("search x"), &mut env); // view = search
        state.dispatch(&command::parse("info 1"), &mut env);
        assert_eq!(
            env.info_calls.last(),
            Some(&vec![PkgTarget::new("searched")]),
            "in the search view, `1` is the search row"
        );
        state.dispatch(&command::parse("show"), &mut env); // view = cart
        state.dispatch(&command::parse("info 1"), &mut env);
        assert_eq!(
            env.info_calls.last(),
            Some(&vec![PkgTarget::new("staged")]),
            "after `show`, `1` is the cart row"
        );
    }

    // --- undo / redo ---

    #[test]
    fn undo_restores_a_cart_over_narrowed_by_keep() {
        // The reported bug: `keep` dropped more than intended and the rows were
        // gone for good. `undo` brings the whole pre-`keep` cart back.
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Aur),
            ("baz", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar baz"), &mut env);
        state.dispatch(&command::parse("keep bar"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar"]);
        state.dispatch(&command::parse("undo"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["bar", "baz", "foo"],
            "undo restores every row `keep` dropped"
        );
    }

    #[test]
    fn redo_reapplies_an_undone_change() {
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Aur),
            ("baz", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar baz"), &mut env);
        state.dispatch(&command::parse("keep bar"), &mut env);
        state.dispatch(&command::parse("undo"), &mut env);
        state.dispatch(&command::parse("redo"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar"], "redo reapplies the keep");
    }

    #[test]
    fn undo_steps_back_one_change_at_a_time() {
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("add bar"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar", "foo"]);
        state.dispatch(&command::parse("undo"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "one undo reverts `add bar`"
        );
        state.dispatch(&command::parse("undo"), &mut env);
        assert!(state.cart.is_empty(), "the next undo reverts `add foo`");
    }

    #[test]
    fn a_fresh_change_forgets_the_redo_branch() {
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Aur),
            ("qux", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        state.dispatch(&command::parse("drop foo"), &mut env);
        state.dispatch(&command::parse("undo"), &mut env); // redo branch now holds the post-drop cart
        state.dispatch(&command::parse("add qux"), &mut env); // a new edit forks the branch
        env.lines.clear();
        state.dispatch(&command::parse("redo"), &mut env);
        assert!(
            env.lines.contains("nothing to redo"),
            "the redo branch was discarded by the new edit: {:?}",
            env.lines
        );
    }

    #[test]
    fn undo_with_no_history_is_a_friendly_no_op() {
        let (_flow, env) = dispatch_one("undo");
        assert!(env.lines.contains("nothing to undo"));
    }

    #[test]
    fn redo_with_no_undone_change_is_a_friendly_no_op() {
        let (_flow, env) = dispatch_one("redo");
        assert!(env.lines.contains("nothing to redo"));
    }

    #[test]
    fn clear_is_undoable() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("clear"), &mut env);
        assert!(state.cart.is_empty());
        state.dispatch(&command::parse("undo"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "undo brings back a cleared cart"
        );
    }

    #[test]
    fn a_clean_apply_forgets_the_undo_history() {
        // After a transaction runs, the snapshots describe an old world (they'd
        // re-stage now-installed packages), so `apply` drops them.
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("approve foo"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env); // FakeEnv default: Succeeded
        assert!(state.cart.is_empty(), "a clean apply empties the cart");
        env.lines.clear();
        state.dispatch(&command::parse("undo"), &mut env);
        assert!(
            env.lines.contains("nothing to undo"),
            "apply cleared the undo history: {:?}",
            env.lines
        );
    }

    #[test]
    fn expand_repo_tokens_expands_known_repos_and_passes_others_through() {
        let rows = vec![
            RepoRow {
                target: PkgTarget::new("glibc"),
                repo: Some(RepoName::from("core")),
            },
            RepoRow {
                target: PkgTarget::new("yay-bin"),
                repo: Some(RepoName::from("aur")),
            },
        ];
        // A repo name expands to its rows' targets…
        assert_eq!(expand_repo_tokens(&[s("aur")], &rows), vec!["yay-bin"]);
        // …case-insensitively…
        assert_eq!(expand_repo_tokens(&[s("CORE")], &rows), vec!["glibc"]);
        // …while numbers, names, and globs pass through untouched.
        assert_eq!(expand_repo_tokens(&[s("3")], &rows), vec!["3"]);
        assert_eq!(expand_repo_tokens(&[s("nginx")], &rows), vec!["nginx"]);
        assert_eq!(expand_repo_tokens(&[s("py-*")], &rows), vec!["py-*"]);
    }

    fn s(t: &str) -> String {
        t.to_owned()
    }
}
