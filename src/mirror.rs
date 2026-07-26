//! Bare clone of the AUR mirror plus per-pkgbase build-directory materialization.
//!
//! Built on [`gix`] (gitoxide), pure Rust. No subprocess, no libgit2.
//! Per-pkgbase directories are *materialized* from the bare repo's tree
//! objects rather than created via `git worktree add` — aurox owns those
//! directories, so a plain checkout is sufficient.

use crate::config::Config;
use crate::context;
use crate::error::{Error, Result};
use crate::git;
use crate::index;
use crate::pacman::sync::{self, SyncOutcome};
use crate::paths;
use crate::ui;
use gix::protocol::transport::client::blocking_io::http;
use indicatif::MultiProgress;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;

pub mod clone;
mod consent;
pub mod fetch;
pub mod sideband;
pub mod worktree;

use consent::AurAction;
pub use consent::{RefreshReason, SkipCause};

/// Which package sources one [`cmd_refresh`] covers.
///
/// The CLI (`-Sy`) and the shell's bare `refresh` cover everything; the
/// shell's `refresh aur` / `refresh pacman` narrow it to one source. Scope is
/// orthogonal to [`RefreshReason`], which picks how a needed AUR bootstrap
/// obtains consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshScope {
    /// AUR mirror + index, and (unless [`Config::check_repo_updates`] is off)
    /// the official-repo sync DBs.
    Everything,
    /// Only the AUR mirror + index.
    Aur,
    /// Only the official-repo sync DBs; the AUR mirror is left alone —
    /// consent included, so this can never prompt for a bootstrap.
    Pacman,
}

/// What **one package source** did in a refresh.
///
/// Every source reports the same way, and that rule is the type: a source that
/// **refreshed** narrates itself as it goes (the AUR fetch's progress rows, the
/// sync DBs' "refreshed / up to date / failed" note), and one that **didn't**
/// carries a typed cause out to the caller, who alone knows how to word it in
/// its own vocabulary. So a skip is data everywhere, never a `bool` that
/// decides control flow and prints its explanation at the decision site.
///
/// Generic over the cause because the *shape* is shared while the *reasons*
/// aren't: the AUR source can be skipped by a consent answer, which has no
/// counterpart for the sync DBs, and `Disabled` names a different config knob
/// on each side. Sharing one cause enum would let a source claim a reason that
/// cannot apply to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOutcome<C> {
    /// The source ran; it reported its own result as it went.
    Refreshed,
    /// It didn't run, for this reason.
    Skipped(C),
}

/// What one refresh did, source by source.
///
/// Two named sources today. A **third** source is a new field — every `match`
/// on this struct is then a compile error until it's handled, which is the
/// point of naming them rather than keying a map. Letting the user refresh
/// *part* of pacman (`refresh core extra`) doesn't add a field either: it adds
/// detail inside [`Self::repo`]'s `Refreshed`, where the per-db results
/// already live (today they're only printed). See docs/TODO.md's
/// one-source-type item — when that lands, these fields are what it keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub aur: SourceOutcome<SkipCause>,
    pub repo: SourceOutcome<RepoSkip>,
}

impl RefreshOutcome {
    /// Every source refreshed — the shape a test or a caller wants when
    /// nothing was skipped.
    pub const REFRESHED: Self = Self {
        aur: SourceOutcome::Refreshed,
        repo: SourceOutcome::Refreshed,
    };
}

/// Why the official-repo source was skipped — the counterpart of
/// [`SkipCause`], kept separate because the two are skipped for genuinely
/// different reasons (no consent question exists here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoSkip {
    /// `check_repo_updates = false` in config.toml.
    Disabled,
    /// The command's [`RefreshScope`] excluded it (`refresh aur`).
    NotRequested,
}

impl std::fmt::Display for RepoSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "check_repo_updates = false in config",
            Self::NotRequested => "not requested",
        })
    }
}

/// What the repo source was asked to do — decided before any work starts, so
/// the skip is a value rather than a branch taken at the spawn site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoPlan {
    Sync,
    Skip(RepoSkip),
}

impl RepoPlan {
    /// Pure decision, parameters injected (like [`consent::decide`]): the
    /// scope wins first — `refresh aur` asked for the AUR and nothing else —
    /// then the config knob.
    const fn decide(check_repo_updates: bool, scope: RefreshScope) -> Self {
        match scope {
            RefreshScope::Aur => Self::Skip(RepoSkip::NotRequested),
            RefreshScope::Everything | RefreshScope::Pacman if !check_repo_updates => {
                Self::Skip(RepoSkip::Disabled)
            }
            _ => Self::Sync,
        }
    }
}

/// Build the `http::Options` payload gix's curl transport downcasts in its
/// `configure()` hook. Sets `lowSpeedLimit=1`, `lowSpeedTime=idle_timeout_secs`
/// so the connection aborts after `idle_timeout_secs` of <1 byte/s — i.e., true
/// silence from the remote, not a total deadline (0 disables the guard).
/// Callers pick the window per phase: incremental fetches pass
/// `cfg.mirror_idle_timeout_secs`, the bootstrap clone the far larger
/// `cfg.bootstrap_idle_timeout_secs` (see its doc for why). `download_progress`
/// is the counter the backend adds each received body chunk to, driving the
/// UI's `network` throughput row (the only live signal during the
/// otherwise-silent ls-refs advertisement).
pub(crate) fn http_transport_options(
    idle_timeout_secs: u64,
    download_progress: Arc<AtomicU64>,
    should_interrupt: Arc<AtomicBool>,
) -> http::Options {
    let mut opts = http::Options::default();
    if idle_timeout_secs > 0 {
        opts.low_speed_limit_bytes_per_second = 1;
        opts.low_speed_time_seconds = idle_timeout_secs;
    }
    opts.download_progress = Some(download_progress);
    // The same flag `cancel_on_sigint` hands to gix's cooperative `receive`,
    // also given to the curl backend: gix's check only fires between reads /
    // during its CPU phases, so the curl transfer meter is what aborts a Ctrl+C
    // while gix is parked in a read on an idle or slow socket.
    opts.should_interrupt = Some(should_interrupt);
    opts
}

/// `set_transport_options` wants `Box<dyn Any>`; wrap once at the call site.
pub(crate) fn boxed_http_options(
    idle_timeout_secs: u64,
    download_progress: Arc<AtomicU64>,
    should_interrupt: Arc<AtomicBool>,
) -> Box<dyn Any + Send + Sync> {
    Box::new(http_transport_options(
        idle_timeout_secs,
        download_progress,
        should_interrupt,
    ))
}

/// Handle to the bare AUR mirror on disk.
pub struct MirrorRepo {
    /// On-disk path of the bare repo.
    pub path: PathBuf,
    /// Open gix repo. `gix::Repository` is `Send`+`Sync` so workers may share it.
    pub repo: gix::Repository,
}

impl MirrorRepo {
    /// Open the existing bare clone at `path` without any network access.
    pub fn open(path: &Path) -> Result<Self> {
        let repo =
            gix::open(path).map_err(|e| Error::gix(format_args!("open {}", path.display()), e))?;
        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }

    /// Refresh the mirror's commit-graph so the *next* fetch's negotiation can
    /// read commit times from an mmap'd file instead of inflating every commit
    /// from the pack (the dominant remaining cost of building the have-set).
    ///
    /// `new_commits` is forwarded to [`crate::git::write_commit_graph`]:
    /// `Some(tips)` for an incremental fetch (only those tips' closure is
    /// graphed), `None` for a fresh clone / full rebuild (walk every ref).
    /// Best-effort — see [`crate::git::write_commit_graph`].
    pub fn write_commit_graph(&self, new_commits: Option<&[gix::ObjectId]>) {
        git::write_commit_graph(&self.path, new_commits);
    }
}

/// Refresh aurox's databases, per `scope`: the AUR mirror — subject to the
/// bootstrap consent gate — and, unless [`Config::check_repo_updates`] is
/// off, the official-repo sync DBs in parallel.
///
/// Both halves draw into one shared [`MultiProgress`] so the AUR fetch rows and
/// the per-repo db-download rows line up in a single display. The repo sync is
/// best-effort: a failure there is reported as a warning and never fails the
/// AUR refresh (whose result is what this returns). A scope that excludes the
/// AUR source returns [`SkipCause::NotRequested`] without ever consulting the
/// consent gate.
///
/// `reason` says who asked (see [`RefreshReason`]): [`RefreshReason::ForceReclone`]
/// (`aurox -Syy`) blows away the existing bare clone and re-bootstraps from
/// scratch, and the reason also picks how consent for a needed bootstrap is
/// obtained — the ~2 GiB clone never starts without a yes. A decline (or
/// `aur = false` in config.toml) still refreshes the sync DBs and returns
/// [`SourceOutcome::Skipped`] so callers can hint at what was skipped.
pub fn cmd_refresh(
    cfg: &Config,
    reason: RefreshReason,
    scope: RefreshScope,
) -> Result<RefreshOutcome> {
    // Resolve consent before the progress display exists: dialoguer and
    // indicatif both draw on the terminal, and a prompt under live progress
    // rows gets clobbered by redraws.
    let action = if scope == RefreshScope::Pacman {
        AurAction::Skip(SkipCause::NotRequested)
    } else {
        consent::plan(cfg, reason)?
    };
    // Decided next to the AUR source and *as a value*, so "the repo DBs weren't
    // refreshed, because X" travels out with the outcome instead of being
    // printed at the branch that skipped them.
    let repo_plan = RepoPlan::decide(cfg.check_repo_updates, scope);
    let mp = MultiProgress::new();
    let (aur, repo) = match repo_plan {
        // Scoped thread: the official-repo db sync (libalpm download) overlaps
        // the network-bound AUR fetch. It borrows `cfg`/`mp` for the scope and
        // draws its own rows into the shared display.
        RepoPlan::Sync => context::scope(|s| {
            let handle = s.spawn(|| sync::refresh_sync_db(&mp));
            let aur = run_aur_action(cfg, action, &mp);
            report_repo_sync(handle.join());
            (aur, SourceOutcome::Refreshed)
        }),
        // No thread for work that isn't happening — but the *outcome* is the
        // same shape either way.
        RepoPlan::Skip(cause) => (
            run_aur_action(cfg, action, &mp),
            SourceOutcome::Skipped(cause),
        ),
    };
    // Backstop: wipe any progress rows a mid-download error may have left
    // (each row normally clears itself on completion).
    mp.clear().ok();
    // A successful refresh stamps the TTL that the shell's `upgrade` honours.
    // Deliberately stamped on `AurSkipped` too: a declined bootstrap must not
    // re-prompt on every TTL-driven `upgrade` within the window. A repo-only
    // scope never stamps — the mirror wasn't touched, and claiming otherwise
    // would make `upgrade` skip a fetch the user still needs.
    if scope != RefreshScope::Pacman && aur.is_ok() {
        record_fetch_stamp();
    }
    Ok(RefreshOutcome { aur: aur?, repo })
}

/// Record "the mirror was fetched just now" so the shell's `upgrade` can skip a
/// redundant fetch within [`Config::refresh_max_age_secs`]. Best-effort: a write
/// failure just means the next `upgrade` re-fetches (the pre-TTL behaviour),
/// never an error. See [`paths::fetch_stamp_path`] for why this is a stamp file
/// rather than an artifact mtime.
fn record_fetch_stamp() {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Err(e) = std::fs::write(paths::fetch_stamp_path(), secs.to_string()) {
        debug!(error = %e, "record AUR fetch stamp");
    }
}

/// How long ago the AUR mirror was last fetched, per the stamp
/// [`record_fetch_stamp`] writes. `None` when it was never fetched (no stamp) or
/// the stamp is unreadable/garbled — the caller then treats the mirror as stale
/// and fetches, matching the always-fetch behaviour that predated the TTL. A
/// future stamp (the clock moved backwards) reads as a zero age rather than
/// re-fetching on every `upgrade`.
pub(crate) fn last_fetch_age() -> Option<Duration> {
    let raw = std::fs::read_to_string(paths::fetch_stamp_path()).ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    let stamped = UNIX_EPOCH + Duration::from_secs(secs);
    Some(
        SystemTime::now()
            .duration_since(stamped)
            .unwrap_or(Duration::ZERO),
    )
}

/// Surface the parallel repo-db sync's outcome once the shared progress display
/// is torn down. Best-effort — every failure mode is a warning, never fatal.
fn report_repo_sync(joined: std::thread::Result<Result<SyncOutcome>>) {
    match joined {
        Ok(Ok(SyncOutcome::Refreshed)) => ui::note("official package databases refreshed"),
        Ok(Ok(SyncOutcome::AlreadyCurrent)) => ui::note("official package databases up to date"),
        // Ctrl+C during the sync — waiting out a concurrent refresh's advisory
        // lock, or mid-download (the fetcher aborts the transfer). Deliberate,
        // not a failure.
        Ok(Err(Error::Interrupted)) => ui::note("official-repo refresh interrupted"),
        Ok(Err(e)) => ui::warn(&format!("official-repo refresh failed: {e}")),
        Err(_) => ui::warn("official-repo refresh thread panicked"),
    }
}

/// Execute the consented AUR source of one refresh, drawing progress into the
/// shared `mp`.
fn run_aur_action(
    cfg: &Config,
    action: AurAction,
    mp: &MultiProgress,
) -> Result<SourceOutcome<SkipCause>> {
    match action {
        AurAction::Skip(cause) => Ok(SourceOutcome::Skipped(cause)),
        AurAction::Bootstrap(_) => {
            bootstrap_aur(cfg, mp)?;
            Ok(SourceOutcome::Refreshed)
        }
        AurAction::Fetch => {
            fetch_aur(cfg, mp)?;
            Ok(SourceOutcome::Refreshed)
        }
    }
}

/// Full bootstrap: wipe whatever is on disk (an interrupted clone or a
/// force-recloned mirror), clone from scratch, build the index, seed the
/// commit-graph. Consent — including for the wipe — was already obtained in
/// [`consent::plan`], which also announced what is about to happen.
fn bootstrap_aur(cfg: &Config, mp: &MultiProgress) -> Result<()> {
    let path = paths::aur_repo_path();
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    clone::bootstrap_clone(cfg, &path, mp)?;
    ui::info("building index");
    let mirror = MirrorRepo::open(&path)?;
    let idx = index::build::full_build(cfg, &mirror)?;
    index::save(&idx, &paths::index_path())?;
    ui::info("index built");
    // Seed the commit-graph so the first incremental `-Sy` negotiates fast.
    // Fresh clone: no delta, so walk every ref (`--reachable`).
    mirror.write_commit_graph(None);
    Ok(())
}

/// Fetch AUR mirror updates and incrementally refresh the on-disk index,
/// drawing progress into the shared `mp`.
fn fetch_aur(cfg: &Config, mp: &MultiProgress) -> Result<()> {
    let path = paths::aur_repo_path();
    ui::info("refreshing AUR mirror");
    let mirror = MirrorRepo::open(&path)?;
    let idx_path = paths::index_path();

    // The fetch is network-bound and the index load is local file I/O against
    // a different file, so run them concurrently: the ~0.5s load disappears
    // under the multi-second fetch. A scoped thread lets the loader borrow
    // `&idx_path` without an `Arc`; the fetch keeps the foreground (and its
    // progress UI) on this thread.
    //
    // A failed load (rkyv validation, schema mismatch after a aurox upgrade,
    // etc.) is **recovered from in-place** by falling back to a full rebuild
    // below — otherwise the user would be stuck in a loop where `-Sy` errors
    // out before it can rebuild.
    let (updates, existing) = context::scope(|s| {
        let loader = s.spawn(|| {
            if !idx_path.exists() {
                return None;
            }
            match index::load(&idx_path) {
                Ok(idx) => Some(idx),
                Err(e) => {
                    // Expected after a aurox upgrade bumps the schema: the
                    // rebuild below is announced by "building index"/"index
                    // built", and on the resync path `load_or_resync` has
                    // already told the user why. So this is a trace, not a
                    // user-facing warning.
                    debug!(error = %e, "existing index unreadable; rebuilding from scratch");
                    None
                }
            }
        });
        let updates = fetch::incremental_fetch(cfg, &mirror, mp)?;
        let existing = loader
            .join()
            .expect("the index loader thread should not panic");
        Ok::<_, Error>((updates, existing))
    })?;

    match existing {
        Some(mut idx) if !updates.is_empty() => {
            index::update::incremental_update(&mirror, &updates, &mut idx)?;
            index::save(&idx, &idx_path)?;
            ui::note(&format!("{} ref(s) updated", updates.len()));
            // New commits arrived; fold them into the commit-graph for next
            // time. Feed just the fetched tips (`--stdin-commits`) so git
            // graphs their closure instead of re-walking all ~155k refs.
            let tips: Vec<gix::ObjectId> = updates.iter().filter_map(|u| u.new_oid).collect();
            mirror.write_commit_graph(Some(&tips));
        }
        Some(_) => {
            // Nothing changed on the mirror, so the commit-graph is still current.
            ui::note("no ref updates");
        }
        None => {
            ui::info("building index");
            let idx = index::build::full_build(cfg, &mirror)?;
            index::save(&idx, &idx_path)?;
            ui::info("index built");
            // Full rebuild: graph the whole history (`--reachable`).
            mirror.write_commit_graph(None);
        }
    }
    Ok(())
}

/// A bare clone counts as "bootstrapped" if it has at least one branch under
/// `refs/heads/*`. gix writes refs after the pack is durable, so absence of
/// refs ⇒ the previous clone never finished.
fn is_bootstrapped(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(repo) = gix::open(path) else {
        return false;
    };
    let Ok(refs) = repo.references() else {
        return false;
    };
    let Ok(mut iter) = refs.prefixed("refs/heads/") else {
        return false;
    };
    iter.next().is_some()
}

#[cfg(test)]
mod tests {
    use super::{RefreshScope, RepoPlan, RepoSkip};

    /// The repo source's decision, over the whole (scope × knob) matrix. Pure and
    /// parameter-injected like `consent::decide`, so the table *is* the test —
    /// and the two skip reasons stay distinguishable, which is the whole point
    /// of it being a value rather than a `bool`.
    #[test]
    fn repo_plan_decides_by_scope_then_knob() {
        use RefreshScope::{Aur, Everything, Pacman};
        let cases = [
            // (check_repo_updates, scope, expected)
            (true, Everything, RepoPlan::Sync),
            (true, Pacman, RepoPlan::Sync),
            // Naming the AUR excludes the repo source whatever the knob says.
            (true, Aur, RepoPlan::Skip(RepoSkip::NotRequested)),
            (false, Aur, RepoPlan::Skip(RepoSkip::NotRequested)),
            // The knob is what's left, and it keeps its own reason — an
            // explicitly repo-scoped refresh with the knob off must be able to
            // say *why* nothing happened.
            (false, Everything, RepoPlan::Skip(RepoSkip::Disabled)),
            (false, Pacman, RepoPlan::Skip(RepoSkip::Disabled)),
        ];
        for (knob, scope, want) in cases {
            assert_eq!(
                RepoPlan::decide(knob, scope),
                want,
                "check_repo_updates = {knob}, scope = {scope:?}"
            );
        }
    }
}
