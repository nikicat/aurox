//! Upgrade-procedure support for the shell.
//!
//! The cart replaces the old `upgrade_loop` driver (refresh → picker → apply
//! loop), but the loop's *reusable* machinery lives on here: the refresh+reload
//! step, the dep-row extraction the unified `show` table renders, and the
//! build-time cost overlay. The shell's `show` feeds these into
//! [`ui::transaction_table`]; `apply` feeds them into [`ui::cost_summary`].

use crate::build;
use crate::build::UpgradeSession;
use crate::build::metrics::MetricsStore;
use crate::config::Config;
use crate::error::Result;
use crate::mirror;
use crate::names::{PkgBase, PkgName, RepoRank};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::paths;
use crate::resolver::Plan;
use crate::ui::{PreviewMetrics, TxnRoot};
use std::collections::HashSet;
use std::time::SystemTime;
use tracing::warn;

/// Refresh the mirror + index, then reload the session so subsequent
/// `search`/`info`/`upgrade` see fresh data. `Ok(None)` when no index exists
/// even after the refresh (shouldn't happen once a clone is on disk, but the
/// caller degrades gracefully).
pub(crate) fn refresh_and_reload(cfg: &Config) -> Result<Option<UpgradeSession>> {
    mirror::cmd_refresh(cfg, false)?;
    UpgradeSession::load(cfg)
}

/// A system-dbpath pacman snapshot — `pacman -S/-U/-Syu` act against this db, so
/// the apply plan must match what pacman will see.
pub(crate) fn system_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open()?;
    Ok(PacmanIndex::build(&alpm))
}

/// A rootless-synced snapshot, used only for the preview's *size* figures: the
/// candidate versions came from this freshly-synced db, so its syncdb carries
/// the new versions whose archives aren't cached — `download_size()` then
/// reports the real fetch cost rather than the `0 B` the stale system syncdb
/// yields for an already-cached installed version.
pub(crate) fn synced_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open_synced()?;
    Ok(PacmanIndex::build(&alpm))
}

/// How old a recorded build duration may be before the preview dims it. Build
/// flows drift as compilers/libs/ccache move on, so a months-old measurement is
/// a shaky predictor; 90 days balances "covers routine rebuilds" against
/// "anything older deserves the dim".
const STALE_METRIC_AGE_SECS: u64 = 90 * 24 * 3_600;

/// The dep rows the unified table shows beneath the roots: concrete repo
/// pkgnames the build pulls in, plus AUR pkgbases that were dragged in rather
/// than named.
pub(crate) fn dep_rows(plan: Option<&Plan>) -> (Vec<PkgName>, Vec<PkgBase>) {
    let Some(plan) = plan else {
        return (Vec::new(), Vec::new());
    };
    let repo_deps = plan.transitive_repo.iter().map(PkgName::from).collect();
    let root_bases: HashSet<&PkgBase> = plan.direct_aur.iter().collect();
    let aur_deps = plan
        .aur_strata
        .iter()
        .flatten()
        .filter(|pb| !root_bases.contains(pb))
        .cloned()
        .collect();
    (repo_deps, aur_deps)
}

/// Build the per-row build-time + `built` overlay for the preview from the
/// cross-session metrics store. Empty overlay on any store failure (the preview
/// then shows `~?` for AUR rows, which is correct and never blocks apply).
pub(crate) fn preview_metrics(
    session: &UpgradeSession,
    roots: &[TxnRoot],
    plan: Option<&Plan>,
) -> PreviewMetrics {
    let store = MetricsStore::open(&paths::metrics_db_path())
        .inspect_err(
            |e| warn!(error = %e, "open metrics store for preview; skipping build-time column"),
        )
        .ok();
    let mut out = PreviewMetrics::empty();
    let now = SystemTime::now();

    // AUR roots only — repo upgrades have no build, so no build-time/built cell.
    for r in roots.iter().filter(|r| r.repo.rank() == RepoRank::Aur) {
        let Some(pb) = session.pkgbase_of(&r.name) else {
            continue;
        };
        if row_built(pb, r) {
            out.built_roots.insert(r.name.clone());
        }
        if let Some(store) = &store {
            insert_root_time(&mut out, store, &r.name, pb, now);
        }
    }

    // Pulled-in AUR deps: pkgbases in the strata that weren't named roots.
    if let Some(plan) = plan {
        let root_bases: HashSet<&PkgBase> = plan.direct_aur.iter().collect();
        let dep_pkgbases: Vec<&PkgBase> = plan
            .aur_strata
            .iter()
            .flatten()
            .filter(|pb| !root_bases.contains(pb))
            .collect();
        for pb in &dep_pkgbases {
            if pkgbase_built(session, pb) {
                out.built_deps.insert((*pb).clone());
            }
        }
        if let Some(store) = &store {
            match store.latest_build_many(dep_pkgbases.iter().copied()) {
                Ok(map) => {
                    out.dep_build_secs = map
                        .into_iter()
                        .map(|(pb, rec)| (pb, rec.build_secs))
                        .collect();
                }
                Err(e) => warn!(error = %e, "bulk lookup AUR dep build times"),
            }
        }
    }
    out
}

/// Whether a single AUR root row's artifact is already built on disk
/// (per-pkgname, like the picker's `built` tag — see [`build::artifacts_built`]).
/// A row with no resolved `new_ver` (couldn't look one up) counts as not built.
fn row_built(pb: &PkgBase, r: &TxnRoot) -> bool {
    r.new_ver
        .as_ref()
        .is_some_and(|v| build::artifacts_built(pb, v, std::slice::from_ref(&r.name)))
}

/// Record `pkgbase`'s latest build duration into the root overlay keyed by
/// `name`, flagging it stale past [`STALE_METRIC_AGE_SECS`]. A lookup error
/// warns and leaves the row at `~?`; a clock-skew age error under-dims rather
/// than wrongly disparaging a fresh figure.
fn insert_root_time(
    out: &mut PreviewMetrics,
    store: &MetricsStore,
    name: &PkgName,
    pb: &PkgBase,
    now: SystemTime,
) {
    match store.latest_build(pb) {
        Ok(Some(rec)) => {
            out.root_build_secs.insert(name.clone(), rec.build_secs);
            if rec.age(now).is_ok_and(|a| a >= STALE_METRIC_AGE_SECS) {
                out.stale.insert(name.clone());
            }
        }
        Ok(None) => {}
        Err(e) => warn!(error = %e, pkgbase = %pb, "lookup AUR root build time"),
    }
}

/// Whether `pkgbase`'s build is complete on disk — every pkgname it produces at
/// the index version has an artifact. Used for the pulled-in AUR **dep** rows
/// (labelled by pkgbase, unlike per-pkgname roots). `false` when the pkgbase
/// isn't in the index.
fn pkgbase_built(session: &UpgradeSession, pb: &PkgBase) -> bool {
    session
        .secondary()
        .lookup_pkgbase(session.index(), pb)
        .is_some_and(|entry| {
            let pkgnames: Vec<PkgName> = entry.pkgnames.iter().map(|p| p.name.clone()).collect();
            build::artifacts_built(pb, &entry.version(), &pkgnames)
        })
}
