//! Pre-apply change-set preview for the upgrade loop.
//!
//! Right before `pacman -Syu` / `pacman -U`, the loop renders this preview so
//! the user sees what's about to happen and what it will cost.
//!
//! Two cost figures live here:
//! - **Size** (phase 2 of `docs/UPDATE_LOOP.md`) — exact `download_size` from
//!   the syncdb for repo rows; `~`-estimated `isize` from localdb for AUR
//!   rows; `~?` for never-installed pull-ins.
//! - **Build time** (phase 3) — `~Xm Ys` from the cross-session
//!   `MetricsStore` for AUR rows that have ever been built before; `~?` for
//!   first-time builds the store can't predict; dimmed when the localdb
//!   install date is old enough that the recorded duration is from a
//!   different build flow.
//!
//! Split out of `tables.rs` (which still owns the picker / upgrade-table
//! primitives) because the change-set rendering carries its own type cluster
//! — `SizeEst`, `TimeEst`, `PreviewMetrics` — that doesn't apply anywhere
//! else.

use super::cost::{PreviewMetrics, RowCost, TimeEst, built_suffix, cost_of_root, time_col};
use super::tables::{Paint, col_widths, render_row, sort_for_display};
use super::{color_on, human_bytes, human_duration};
use crate::names::{PkgBase, PkgName};
use crate::pacman::alpm_db::PacmanIndex;
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use std::fmt::Write;

/// Render the change-set preview.
///
/// `roots` are the selected upgrade rows (repo + AUR mixed); `repo_deps` are
/// concrete pkgnames `pacman -S` will install on top; `aur_deps` are the extra
/// AUR pkgbases the resolver pulled in; `pac` is the snapshot the size figures
/// are read from; `metrics` carries the per-AUR-row build-time overlay.
pub fn change_set_table(
    roots: &[PkgUpgrade],
    repo_deps: &[PkgName],
    aur_deps: &[PkgBase],
    pac: &PacmanIndex,
    metrics: &PreviewMetrics,
) {
    let dep_count = repo_deps.len() + aur_deps.len();
    let header = if dep_count == 0 {
        format!("this batch — {} package(s)", roots.len())
    } else {
        format!(
            "this batch — {} package(s), +{dep_count} dependenc{}",
            roots.len(),
            if dep_count == 1 { "y" } else { "ies" },
        )
    };
    super::info(&header);

    let ordered = sort_for_display(roots);
    let (repo_w, name_w, old_w) = col_widths(&ordered);
    let new_w = ordered.iter().map(|u| u.new_ver.len()).max().unwrap_or(0);
    let paint = Paint::from(color_on());

    // Resolve every row's size once: the figures drive the size-column width,
    // the per-row cells, and the batch total in a single pass.
    let root_sizes: Vec<SizeEst> = ordered.iter().map(|u| size_of_root(u, pac)).collect();
    let repo_dep_sizes: Vec<SizeEst> = repo_deps.iter().map(|n| size_of_repo_dep(n, pac)).collect();
    // Pulled-in AUR deps are unsatisfied builds — not yet installed — so their
    // footprint is unknown (`~?`). See `docs/UPDATE_LOOP.md` § Cost estimates.
    let aur_dep_sizes: Vec<SizeEst> = vec![SizeEst::Unknown; aur_deps.len()];
    let size_w = root_sizes
        .iter()
        .chain(&repo_dep_sizes)
        .chain(&aur_dep_sizes)
        .map(|s| s.render().len())
        .max()
        .unwrap_or(0);

    // Build-time + built state per row. Repo rows are [`RowCost::none`] (empty
    // cell, no tag); AUR rows carry their Estimate / Unknown figure, the stale
    // dim flag, and whether the artifact is already built.
    let colored = paint.colored();
    let root_costs: Vec<RowCost> = ordered.iter().map(|u| cost_of_root(u, metrics)).collect();
    let aur_dep_costs: Vec<RowCost> = aur_deps
        .iter()
        .map(|pb| cost_of_aur_dep(pb, metrics))
        .collect();
    let time_w = root_costs
        .iter()
        .chain(&aur_dep_costs)
        .map(|c| c.visible_len())
        .max()
        .unwrap_or(0);

    for ((u, size), cost) in ordered.iter().zip(&root_sizes).zip(&root_costs) {
        let row = render_row(u, repo_w, name_w, old_w, paint);
        let new_pad = " ".repeat(new_w.saturating_sub(u.new_ver.len()));
        eprintln!(
            "    {row}{new_pad}  {size:>size_w$}  {time}{tag}",
            size = size.render(),
            time = time_col(*cost, time_w, colored),
            tag = built_suffix(*cost, colored),
        );
    }

    if dep_count > 0 {
        super::note("pulls in:");
        let dep_w = repo_deps
            .iter()
            .map(PkgName::len)
            .chain(aur_deps.iter().map(PkgBase::len))
            .max()
            .unwrap_or(0);
        // "(install)" is the widest tag — pad both to it so the size column
        // lines up across install and build rows.
        let tag_w = "(install)".len();
        for (name, size) in repo_deps.iter().zip(&repo_dep_sizes) {
            // Repo deps don't build — render the time cell as empty padding so
            // the AUR-dep rows below still align.
            eprintln!(
                "      {name:<dep_w$}  {tag:<tag_w$}  {size:>size_w$}  {empty:>time_w$}",
                tag = "(install)",
                size = size.render(),
                empty = "",
            );
        }
        for ((name, size), cost) in aur_deps.iter().zip(&aur_dep_sizes).zip(&aur_dep_costs) {
            eprintln!(
                "      {name:<dep_w$}  {tag:<tag_w$}  {size:>size_w$}  {time}{built}",
                tag = "(build)",
                size = size.render(),
                time = time_col(*cost, time_w, colored),
                built = built_suffix(*cost, colored),
            );
        }
    }

    let (total_bytes, size_approx) = batch_size_total(
        root_sizes
            .iter()
            .chain(&repo_dep_sizes)
            .chain(&aur_dep_sizes)
            .copied(),
    );
    let (total_secs, time_approx, time_any) =
        batch_time_total(root_costs.iter().chain(&aur_dep_costs).map(|c| c.time));
    let size_prefix = if size_approx { "~" } else { "" };
    let mut total_line = format!("total  {size_prefix}{}", human_bytes(total_bytes));
    // The build-time term joins the size term only when at least one AUR row
    // exists in the batch — pure-repo batches don't need a `0s build` tail.
    if time_any {
        let time_prefix = if time_approx { "~" } else { "" };
        // Write into a String can only fail if the allocator does, which would
        // already be aborting the process — explicit `expect` over a silent
        // `let _ =` makes that contract visible.
        write!(
            total_line,
            "   {time_prefix}{} build",
            human_duration(total_secs),
        )
        .expect("writing to String is infallible");
    }
    super::note(&total_line);
}

/// A change-set row's size figure.
///
/// Repo rows are [`Self::Exact`] (the bytes pacman will download); AUR rows are
/// an [`Self::Estimate`] from the installed version's on-disk size, rendered
/// with a leading `~`; a pulled-in dep that was never installed is
/// [`Self::Unknown`] (`~?`). See `docs/UPDATE_LOOP.md` § Cost estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeEst {
    Exact(u64),
    Estimate(u64),
    Unknown,
}

impl SizeEst {
    /// Bytes this row contributes to the batch total (0 when unknown).
    const fn bytes(self) -> u64 {
        match self {
            Self::Exact(n) | Self::Estimate(n) => n,
            Self::Unknown => 0,
        }
    }

    /// Whether the figure is approximate — any non-exact row makes the batch
    /// total a `~` lower bound.
    const fn approximate(self) -> bool {
        !matches!(self, Self::Exact(_))
    }

    /// The cell text: bare for exact, `~`-prefixed for an estimate, `~?` when
    /// unknown.
    fn render(self) -> String {
        match self {
            Self::Exact(n) => human_bytes(n),
            Self::Estimate(n) => format!("~{}", human_bytes(n)),
            Self::Unknown => "~?".to_owned(),
        }
    }
}

/// Size of a selected root: AUR rows estimate from the installed footprint,
/// repo rows take the exact download size. Either lookup can miss (an AUR pkg
/// pacman has no localdb size for, a repo pkg absent from this db snapshot) →
/// [`SizeEst::Unknown`].
fn size_of_root(u: &PkgUpgrade, pac: &PacmanIndex) -> SizeEst {
    if u.repo == REPO_AUR {
        pac.installed_size(&u.name)
            .map_or(SizeEst::Unknown, SizeEst::Estimate)
    } else {
        pac.sync_download_size(&u.name)
            .map_or(SizeEst::Unknown, SizeEst::Exact)
    }
}

/// Size of a pulled-in repo dependency: the exact bytes `pacman -S` will fetch.
fn size_of_repo_dep(name: &PkgName, pac: &PacmanIndex) -> SizeEst {
    pac.sync_download_size(name)
        .map_or(SizeEst::Unknown, SizeEst::Exact)
}

/// Sum a change set's figures into `(bytes, approximate)`. `bytes` is a lower
/// bound whenever a row is an estimate or unknown; `approximate` flags that so
/// the caller can prefix the total with `~`.
fn batch_size_total(sizes: impl IntoIterator<Item = SizeEst>) -> (u64, bool) {
    let mut total = 0u64;
    let mut approx = false;
    for s in sizes {
        total = total.saturating_add(s.bytes());
        approx |= s.approximate();
    }
    (total, approx)
}

/// Cost cell for one pulled-in AUR build dep — by definition an AUR build, so
/// the figure is Estimate or Unknown (never None). Dep cells aren't dimmed for
/// staleness today, but a built dep still shows the `built` tag.
fn cost_of_aur_dep(pb: &PkgBase, metrics: &PreviewMetrics) -> RowCost {
    let time = metrics
        .dep_build_secs
        .get(pb)
        .copied()
        .map_or(TimeEst::Unknown, TimeEst::Estimate);
    RowCost::aur(time, false, metrics.built_deps.contains(pb))
}

/// Sum a change set's build-time figures into `(seconds, approximate, any)`.
/// `seconds` is a lower bound when any row is `Unknown`; `approximate` flags
/// that so the caller can prefix the total with `~`; `any` reports whether the
/// batch has at least one applicable row (so a pure-repo batch can suppress
/// the trailing `~0s build` term entirely).
fn batch_time_total(times: impl IntoIterator<Item = TimeEst>) -> (u64, bool, bool) {
    let mut total = 0u64;
    let mut approx = false;
    let mut any = false;
    for t in times {
        total = total.saturating_add(t.secs());
        approx |= t.approximate();
        any |= t.applicable();
    }
    (total, approx, any)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Version;

    fn up(repo: &str, name: &str, old: &str, new: &str) -> PkgUpgrade {
        PkgUpgrade {
            repo: repo.into(),
            name: name.into(),
            old_ver: Version::from(old),
            new_ver: Version::from(new),
        }
    }

    /// Each `SizeEst` variant renders its expected cell: bare exact,
    /// `~`-prefixed estimate, `~?` for unknown.
    #[test]
    fn size_est_renders_each_variant() {
        assert_eq!(SizeEst::Exact(1024).render(), "1.00 KiB");
        assert_eq!(SizeEst::Estimate(1024).render(), "~1.00 KiB");
        assert_eq!(SizeEst::Unknown.render(), "~?");
    }

    /// A root's size source is chosen by repo: AUR rows estimate from localdb
    /// `isize`, repo rows take the exact syncdb download size, and a miss in
    /// either map falls back to unknown.
    #[test]
    fn size_of_root_picks_source_by_repo() {
        let mut pac = PacmanIndex::default();
        pac.installed_size
            .insert("paru-bin".into(), 9 * 1024 * 1024);
        pac.sync_download_size
            .insert("glibc".into(), 12 * 1024 * 1024);

        assert_eq!(
            size_of_root(&up(REPO_AUR, "paru-bin", "1-1", "2-1"), &pac),
            SizeEst::Estimate(9 * 1024 * 1024)
        );
        assert_eq!(
            size_of_root(&up("core", "glibc", "2.40-1", "2.41-1"), &pac),
            SizeEst::Exact(12 * 1024 * 1024)
        );
        // AUR row with no localdb size (manually built / never installed).
        assert_eq!(
            size_of_root(&up(REPO_AUR, "ghost", "1-1", "2-1"), &pac),
            SizeEst::Unknown
        );
    }

    /// Regression guard for the stale-db size bug: a repo row whose pkgname is
    /// present in the size index with a `download_size` of 0 (libalpm's answer
    /// for an already-cached archive) is `Exact(0)` → renders `0 B`, a real
    /// value — distinct from a *missing* pkgname, which is `Unknown` → `~?`.
    /// The distinction is why a preview full of `0 B` rows points at the size
    /// source (a stale syncdb whose versions are already cached), not the
    /// formatter.
    #[test]
    fn repo_zero_size_is_exact_not_missing() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("cached".into(), 0);
        let cached = size_of_root(&up("core", "cached", "1-1", "1-2"), &pac);
        assert_eq!(cached, SizeEst::Exact(0));
        assert_eq!(cached.render(), "0 B");
        let missing = size_of_root(&up("core", "absent", "1-1", "1-2"), &pac);
        assert_eq!(missing, SizeEst::Unknown);
        assert_eq!(missing.render(), "~?");
    }

    /// The size total sums every row's bytes and flags itself approximate the
    /// moment a non-exact row (estimate or unknown) is in the mix.
    #[test]
    fn batch_size_total_sums_and_flags_approximate() {
        let (exact, approx) = batch_size_total([SizeEst::Exact(100), SizeEst::Exact(200)]);
        assert_eq!(exact, 300);
        assert!(!approx, "all-exact total must not be marked approximate");

        let (mixed, approx) =
            batch_size_total([SizeEst::Exact(100), SizeEst::Estimate(50), SizeEst::Unknown]);
        assert_eq!(mixed, 150, "unknown contributes 0 to the total");
        assert!(approx, "an estimate makes the total approximate");
    }

    /// Source selection for the root cost cell: repo rows are not-applicable
    /// (`None`, never built); AUR rows resolve their figure through
    /// `root_build_secs`, an unrecorded AUR row is `Unknown`, and the
    /// stale/built flags come straight from the overlay sets.
    #[test]
    fn cost_of_root_picks_by_repo_and_flags() {
        let mut metrics = PreviewMetrics::empty();
        metrics
            .root_build_secs
            .insert(PkgName::from("paru-bin"), 90);
        metrics.built_roots.insert(PkgName::from("paru-bin"));
        metrics.stale.insert(PkgName::from("first-time"));

        // Repo row → no build, so no cell and no flags.
        let repo = cost_of_root(&up("core", "glibc", "1-1", "2-1"), &metrics);
        assert_eq!(repo.time, TimeEst::None);
        assert!(!repo.built);

        // AUR row with a recorded duration, flagged built.
        let recorded = cost_of_root(&up(REPO_AUR, "paru-bin", "1-1", "2-1"), &metrics);
        assert_eq!(recorded.time, TimeEst::Estimate(90));
        assert!(recorded.built);
        assert!(!recorded.stale);

        // AUR row the store has never seen, flagged stale (e.g. from an older
        // sibling measurement) but not built.
        let first = cost_of_root(&up(REPO_AUR, "first-time", "0-1", "1-1"), &metrics);
        assert_eq!(first.time, TimeEst::Unknown);
        assert!(first.stale);
        assert!(!first.built);
    }

    /// Pulled-in AUR build deps don't have a `repo` field to switch on — they
    /// are always AUR builds — so the figure is Estimate or Unknown but never
    /// None; the built flag tracks `built_deps`.
    #[test]
    fn cost_of_aur_dep_resolves_or_unknown() {
        let mut metrics = PreviewMetrics::empty();
        metrics
            .dep_build_secs
            .insert(PkgBase::from("nvidia-utils"), 600);
        metrics.built_deps.insert(PkgBase::from("nvidia-utils"));

        let recorded = cost_of_aur_dep(&PkgBase::from("nvidia-utils"), &metrics);
        assert_eq!(recorded.time, TimeEst::Estimate(600));
        assert!(recorded.built);

        let unknown = cost_of_aur_dep(&PkgBase::from("never-built"), &metrics);
        assert_eq!(unknown.time, TimeEst::Unknown);
        assert!(!unknown.built);
    }

    /// The build-time total reports (sum, approximate, any). `None` doesn't
    /// count toward `any`; either `Estimate` or `Unknown` marks the total
    /// approximate.
    #[test]
    fn batch_time_total_tallies_approx_and_applicability() {
        // Pure-repo batch: no AUR rows ⇒ no build-time term at all.
        let (sec, approx, any) = batch_time_total([TimeEst::None, TimeEst::None]);
        assert_eq!(sec, 0);
        assert!(!approx);
        assert!(!any, "pure-repo batch should suppress the build-time term");

        // Mixed AUR batch: sum, marked approximate, applicable.
        let (sec, approx, any) = batch_time_total([
            TimeEst::Estimate(60),
            TimeEst::Estimate(120),
            TimeEst::Unknown,
            TimeEst::None,
        ]);
        assert_eq!(sec, 180, "unknown contributes 0 but estimates sum");
        assert!(
            approx,
            "an estimate or unknown row makes the total approximate"
        );
        assert!(any);
    }

    /// `change_set_table` must survive the cases most likely to break the
    /// width/zip math: a mixed root + dep batch, the no-deps path, an empty
    /// change set, a stale-marked AUR row, and already-built root + dep rows
    /// (which dim the time cell and append the `built` tag). Output goes to
    /// stderr so we assert "doesn't panic," as the other table smokes do.
    #[test]
    fn change_set_table_smoke() {
        let mut pac = PacmanIndex::default();
        pac.installed_size
            .insert("cuda".into(), 3 * 1024 * 1024 * 1024);
        pac.sync_download_size
            .insert("glibc".into(), 12 * 1024 * 1024);
        pac.sync_download_size
            .insert("gcc13".into(), 50 * 1024 * 1024);
        let roots = vec![
            up(REPO_AUR, "cuda", "12.6-1", "12.8-1"),
            up("core", "glibc", "2.40-1", "2.41-1"),
        ];
        let repo_deps = vec![PkgName::from("gcc13")];
        let aur_deps = vec![PkgBase::from("nvidia-utils")];

        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("cuda"), 2_700);
        metrics
            .dep_build_secs
            .insert(PkgBase::from("nvidia-utils"), 480);
        // Mark cuda's recorded time as stale → the cell renders dimmed.
        metrics.stale.insert(PkgName::from("cuda"));
        // cuda is already built (root) and nvidia-utils too (dep) → both gain a
        // `built` tag, exercising the built rendering in both row kinds.
        metrics.built_roots.insert(PkgName::from("cuda"));
        metrics.built_deps.insert(PkgBase::from("nvidia-utils"));

        change_set_table(&roots, &repo_deps, &aur_deps, &pac, &metrics);
        change_set_table(&roots, &[], &[], &pac, &PreviewMetrics::empty());
        change_set_table(
            &[],
            &[],
            &[],
            &PacmanIndex::default(),
            &PreviewMetrics::empty(),
        );
    }
}
