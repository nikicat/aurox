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

use super::tables::{Paint, col_widths, render_row, sort_for_display};
use super::{color_on, human_bytes, human_duration};
use crate::names::{PkgBase, PkgName};
use crate::pacman::alpm_db::PacmanIndex;
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

/// Per-row build-time overlay the change-set preview reads.
///
/// The maps are keyed differently because AUR root rows arrive as
/// [`PkgUpgrade`] (keyed by [`PkgName`] — what the picker handed us) while AUR
/// build-dep rows arrive as a flat list of [`PkgBase`] (resolver pulls).
/// `stale` marks AUR roots whose recorded build time should render dimmed
/// because the installed version it was measured against is now far away in
/// time.
#[derive(Debug, Default)]
pub struct PreviewMetrics {
    /// AUR root row → last successful build duration (seconds).
    pub root_build_secs: HashMap<PkgName, u64>,
    /// AUR build-dep pkgbase → last successful build duration (seconds).
    pub dep_build_secs: HashMap<PkgBase, u64>,
    /// AUR roots whose localdb install date is older than the staleness
    /// threshold — the recorded `build_secs` reflects a long-ago build flow,
    /// so the cell is dimmed to signal the estimate is shakier than the
    /// number alone suggests.
    pub stale: HashSet<PkgName>,
}

impl PreviewMetrics {
    /// Empty overlay — used by tests and by the upgrade loop when the metrics
    /// store fails to open (every AUR row then renders `~?` for time).
    pub fn empty() -> Self {
        Self::default()
    }
}

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

    // Build-time cells. Repo rows render as [`TimeEst::None`] (empty cell);
    // AUR rows render Estimate / Unknown.
    let root_times: Vec<TimeEst> = ordered.iter().map(|u| time_of_root(u, metrics)).collect();
    let aur_dep_times: Vec<TimeEst> = aur_deps
        .iter()
        .map(|pb| time_of_aur_dep(pb, metrics))
        .collect();
    let time_w = root_times
        .iter()
        .chain(&aur_dep_times)
        .map(|t| t.render().len())
        .max()
        .unwrap_or(0);

    for (i, (u, size)) in ordered.iter().zip(&root_sizes).enumerate() {
        let row = render_row(u, repo_w, name_w, old_w, paint);
        let new_pad = " ".repeat(new_w.saturating_sub(u.new_ver.len()));
        let time = &root_times[i];
        let stale = metrics.stale.contains(&u.name);
        eprintln!(
            "    {row}{new_pad}  {size:>size_w$}  {time:>time_w$}",
            size = size.render(),
            time = time.render_styled(paint, stale),
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
        for ((name, size), time) in aur_deps.iter().zip(&aur_dep_sizes).zip(&aur_dep_times) {
            eprintln!(
                "      {name:<dep_w$}  {tag:<tag_w$}  {size:>size_w$}  {time:>time_w$}",
                tag = "(build)",
                size = size.render(),
                time = time.render_styled(paint, false),
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
        batch_time_total(root_times.iter().chain(&aur_dep_times).copied());
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

/// A change-set row's build-time figure.
///
/// AUR roots and AUR build deps with a recorded prior duration become
/// [`Self::Estimate`] (`~Xm Ys`). AUR rows the store has never seen are
/// [`Self::Unknown`] (`~?`). Repo rows are [`Self::None`] — they don't build
/// at all, so the cell renders empty rather than `~?` (which would imply a
/// missing measurement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeEst {
    Estimate(u64),
    Unknown,
    None,
}

impl TimeEst {
    /// Seconds this row contributes to the batch total (0 when unknown or
    /// not applicable).
    const fn secs(self) -> u64 {
        match self {
            Self::Estimate(n) => n,
            Self::Unknown | Self::None => 0,
        }
    }

    /// Whether the figure makes the build-time total an approximate lower
    /// bound. Both `Estimate` (it's a prediction) and `Unknown` (it under-
    /// counts) flag the total approximate; `None` is "not applicable" and
    /// doesn't affect the total's accuracy.
    const fn approximate(self) -> bool {
        matches!(self, Self::Estimate(_) | Self::Unknown)
    }

    /// Whether this row participates in the build-time total at all. Used to
    /// suppress the trailing `~Xm Ys build` term on pure-repo batches.
    const fn applicable(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Plain cell text. [`Self::None`] returns empty so a padded column
    /// collapses neatly.
    fn render(self) -> String {
        match self {
            Self::Estimate(n) => format!("~{}", human_duration(n)),
            Self::Unknown => "~?".to_owned(),
            Self::None => String::new(),
        }
    }

    /// Whether the rendered cell should be passed through [`super::dim`]:
    /// only when the user can see styling (`paint.colored()`), only when the
    /// estimate is flagged stale, and only on real `Estimate` values — dimming
    /// a `~?` Unknown would look like a render glitch, and there's nothing to
    /// dim on `None`. Pulled out so the decision is testable without
    /// depending on `console`'s global TTY gate.
    const fn should_dim(self, paint: Paint, stale: bool) -> bool {
        paint.colored() && stale && matches!(self, Self::Estimate(_))
    }

    /// Styled cell: same text as [`Self::render`], dimmed when
    /// [`Self::should_dim`] holds. The `to_string` round-trip respects
    /// `console`'s color gate (so a piped output stays plain even when
    /// `stale` is set).
    fn render_styled(self, paint: Paint, stale: bool) -> String {
        let s = self.render();
        if self.should_dim(paint, stale) {
            super::dim(s).to_string()
        } else {
            s
        }
    }
}

/// Build-time cell for one AUR root row. Repo rows return [`TimeEst::None`]
/// (no build happens); AUR rows with a recorded duration become
/// [`TimeEst::Estimate`]; AUR rows the metrics store has never seen are
/// [`TimeEst::Unknown`].
fn time_of_root(u: &PkgUpgrade, metrics: &PreviewMetrics) -> TimeEst {
    if u.repo != REPO_AUR {
        return TimeEst::None;
    }
    metrics
        .root_build_secs
        .get(&u.name)
        .copied()
        .map_or(TimeEst::Unknown, TimeEst::Estimate)
}

/// Build-time cell for one pulled-in AUR build dep. Either Estimate or
/// Unknown — these rows are by definition AUR builds.
fn time_of_aur_dep(pb: &PkgBase, metrics: &PreviewMetrics) -> TimeEst {
    metrics
        .dep_build_secs
        .get(pb)
        .copied()
        .map_or(TimeEst::Unknown, TimeEst::Estimate)
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

    /// `TimeEst` renders the three meaningful cells; `None` collapses to empty
    /// so the column padding does the right thing for repo rows.
    #[test]
    fn time_est_renders_each_variant() {
        assert_eq!(TimeEst::Estimate(45).render(), "~45s");
        assert_eq!(TimeEst::Estimate(125).render(), "~2m 5s");
        assert_eq!(TimeEst::Estimate(3_725).render(), "~1h 2m");
        assert_eq!(TimeEst::Unknown.render(), "~?");
        assert_eq!(TimeEst::None.render(), "");
    }

    /// Source selection for the build-time cell: repo rows are not-applicable
    /// (`None`); AUR rows resolve through `root_build_secs`; an unrecorded
    /// AUR row is `Unknown`.
    #[test]
    fn time_of_root_picks_by_repo_and_lookup() {
        let mut metrics = PreviewMetrics::empty();
        metrics
            .root_build_secs
            .insert(PkgName::from("paru-bin"), 90);

        // Repo row → no build, so no cell.
        assert_eq!(
            time_of_root(&up("core", "glibc", "1-1", "2-1"), &metrics),
            TimeEst::None,
        );
        // AUR row with a recorded duration.
        assert_eq!(
            time_of_root(&up(REPO_AUR, "paru-bin", "1-1", "2-1"), &metrics),
            TimeEst::Estimate(90),
        );
        // AUR row the store has never seen.
        assert_eq!(
            time_of_root(&up(REPO_AUR, "first-time", "0-1", "1-1"), &metrics),
            TimeEst::Unknown,
        );
    }

    /// Pulled-in AUR build deps don't have a `repo` field to switch on — they
    /// are always AUR builds — so the cell is Estimate or Unknown but never
    /// None.
    #[test]
    fn time_of_aur_dep_resolves_or_unknown() {
        let mut metrics = PreviewMetrics::empty();
        metrics
            .dep_build_secs
            .insert(PkgBase::from("nvidia-utils"), 600);

        assert_eq!(
            time_of_aur_dep(&PkgBase::from("nvidia-utils"), &metrics),
            TimeEst::Estimate(600),
        );
        assert_eq!(
            time_of_aur_dep(&PkgBase::from("never-built"), &metrics),
            TimeEst::Unknown,
        );
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

    /// `should_dim` is the decision behind the stale-dim affordance. Only the
    /// exact combination `(Colored, stale=true, Estimate)` qualifies; the
    /// other axes (paint, stale flag, variant) all suppress it.
    #[test]
    fn time_est_should_dim_truth_table() {
        let est = TimeEst::Estimate(60);
        // Only this combination dims.
        assert!(est.should_dim(Paint::Colored, true));
        // Each axis independently disables dimming.
        assert!(
            !est.should_dim(Paint::Plain, true),
            "plain paint must skip dim"
        );
        assert!(
            !est.should_dim(Paint::Colored, false),
            "non-stale must skip dim"
        );
        // Unknown / None must never dim, even with stale=true and Colored.
        assert!(
            !TimeEst::Unknown.should_dim(Paint::Colored, true),
            "Unknown must never dim — `~?` dimmed looks like a render glitch",
        );
        assert!(
            !TimeEst::None.should_dim(Paint::Colored, true),
            "None has no cell to dim",
        );
    }

    /// `change_set_table` must survive the cases most likely to break the
    /// width/zip math: a mixed root + dep batch, the no-deps path, an empty
    /// change set, and a batch with a stale-marked AUR row. Output goes to
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
