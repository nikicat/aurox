//! Per-row cost cells shared by the upgrade picker ([`super::tables`]) and the
//! change-set preview ([`super::change_set`]).
//!
//! A leaf module: it depends only on the `ui` color helpers and the typed
//! names, never on `tables`/`change_set` — so both of those can pull from it
//! without a cycle. Two things live here:
//!
//! - [`PreviewMetrics`] — the per-AUR-row overlay the upgrade loop fills in:
//!   last build duration (the only persisted cost — sizes come straight from
//!   the pacman DBs) and which rows already have artifacts on disk.
//! - [`TimeEst`] — the build-time cell, plus [`built_tag`], the trailing
//!   `built` marker. The size cell ([`super::change_set::SizeEst`]) stays with
//!   the preview because the picker never shows sizes.

use super::human_duration;
use crate::names::{PkgBase, PkgName};
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use console::style;
use std::collections::{HashMap, HashSet};

/// Per-AUR-row cost overlay shared by the picker and the change-set preview.
///
/// Roots are keyed by [`PkgName`] (what the picker hands us) and pulled-in
/// build deps by [`PkgBase`] (what the resolver pulls): the change-set preview
/// reads both, the picker only the root maps. `stale` marks roots whose
/// recorded duration is old enough to render dimmed; `built_*` records the rows
/// whose `.pkg.tar.*` already sit in the build worktree, so a `pacman -U` would
/// reuse them instead of rebuilding.
#[derive(Debug, Default)]
pub struct PreviewMetrics {
    /// AUR root row → last successful build duration (seconds).
    pub root_build_secs: HashMap<PkgName, u64>,
    /// AUR build-dep pkgbase → last successful build duration (seconds).
    pub dep_build_secs: HashMap<PkgBase, u64>,
    /// AUR roots whose recorded `build_secs` is older than the staleness
    /// threshold — the cell is dimmed to signal the estimate is shakier than
    /// the number alone suggests.
    pub stale: HashSet<PkgName>,
    /// AUR root rows whose artifacts already sit in the build worktree.
    pub built_roots: HashSet<PkgName>,
    /// AUR build-dep pkgbases whose artifacts already sit in the worktree.
    pub built_deps: HashSet<PkgBase>,
}

impl PreviewMetrics {
    /// Empty overlay — used by tests, the single-shot `-Syu` picker (which has
    /// no loop session), and the upgrade loop when the metrics store fails to
    /// open (every AUR row then renders `~?` for time and no `built` tag).
    pub fn empty() -> Self {
        Self::default()
    }
}

/// A change-set / picker row's build-time figure.
///
/// AUR roots and AUR build deps with a recorded prior duration become
/// [`Self::Estimate`] (`~Xm Ys`). AUR rows the store has never seen are
/// [`Self::Unknown`] (`~?`). Repo rows are [`Self::None`] — they don't build at
/// all, so the cell renders empty rather than `~?` (which would imply a missing
/// measurement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeEst {
    Estimate(u64),
    Unknown,
    None,
}

impl TimeEst {
    /// Seconds this row contributes to the batch total (0 when unknown or not
    /// applicable).
    pub(super) const fn secs(self) -> u64 {
        match self {
            Self::Estimate(n) => n,
            Self::Unknown | Self::None => 0,
        }
    }

    /// Whether the figure makes the build-time total an approximate lower
    /// bound. Both `Estimate` (a prediction) and `Unknown` (it under-counts)
    /// flag the total approximate; `None` is "not applicable" and doesn't
    /// affect the total's accuracy.
    pub(super) const fn approximate(self) -> bool {
        matches!(self, Self::Estimate(_) | Self::Unknown)
    }

    /// Whether this row participates in the build-time total at all. Used to
    /// suppress the trailing `~Xm Ys build` term on pure-repo batches.
    pub(super) const fn applicable(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Plain canonical cell text — what column widths are measured from.
    /// [`Self::None`] returns empty so a padded column collapses neatly.
    pub(super) fn render(self) -> String {
        match self {
            Self::Estimate(n) => format!("~{}", human_duration(n)),
            Self::Unknown => "~?".to_owned(),
            Self::None => String::new(),
        }
    }

    /// Whether the rendered cell should be passed through [`super::dim`]: only
    /// when the user can see styling (`colored`), only when the figure is
    /// `faded` (stale or already built), and only on a real `Estimate` —
    /// dimming a `~?` Unknown would look like a render glitch, and there's
    /// nothing to dim on `None`. Pulled out so the decision is testable without
    /// depending on `console`'s global TTY gate.
    pub(super) const fn should_dim(self, colored: bool, faded: bool) -> bool {
        colored && faded && matches!(self, Self::Estimate(_))
    }
}

/// One AUR row's resolved cost state for rendering: its build-time figure plus
/// the two display flags that modulate it. Bundled into a named type so the
/// column renderers take one `RowCost` instead of a run of look-alike bools.
///
/// `stale` dims the cell (the measurement is old enough to distrust); `built`
/// means the artifact is already on disk, so the rebuild cost is moot — the
/// cell is dimmed and an `Unknown` collapses to empty (the [`built_suffix`] tag
/// carries the signal instead of a misleading `~?`).
#[derive(Debug, Clone, Copy)]
pub(super) struct RowCost {
    pub(super) time: TimeEst,
    pub(super) stale: bool,
    pub(super) built: bool,
}

impl RowCost {
    /// A repo row: it never builds, so no figure and no flags.
    pub(super) const fn none() -> Self {
        Self {
            time: TimeEst::None,
            stale: false,
            built: false,
        }
    }

    /// An AUR row whose state comes straight from the overlay flags.
    pub(super) const fn aur(time: TimeEst, stale: bool, built: bool) -> Self {
        Self { time, stale, built }
    }

    /// The cell text as it renders for this row. The `to_string` round-trip
    /// respects `console`'s color gate, so piped output stays plain.
    fn cell(self, colored: bool) -> String {
        if self.built && matches!(self.time, TimeEst::Unknown) {
            return String::new();
        }
        let s = self.time.render();
        if self.time.should_dim(colored, self.stale || self.built) {
            super::dim(s).to_string()
        } else {
            s
        }
    }

    /// Visible width of [`Self::cell`] — measured from the plain form so ANSI
    /// escapes in a dimmed cell don't skew column padding. Callers max this
    /// across rows to size the build-time column.
    pub(super) fn visible_len(self) -> usize {
        self.cell(false).len()
    }
}

/// Resolve the [`RowCost`] for one upgrade root from the overlay. Repo rows
/// never build → [`RowCost::none`]; an AUR row takes its recorded duration
/// (`Unknown` when the store has never seen it) plus the stale / already-built
/// flags. Shared by the picker and the change-set preview so a root renders
/// identically in both. Pulled-in AUR *deps* are resolved separately (by
/// pkgbase) in the preview — see `change_set::cost_of_aur_dep`.
pub(super) fn cost_of_root(u: &PkgUpgrade, metrics: &PreviewMetrics) -> RowCost {
    if u.repo != REPO_AUR {
        return RowCost::none();
    }
    let time = metrics
        .root_build_secs
        .get(&u.name)
        .copied()
        .map_or(TimeEst::Unknown, TimeEst::Estimate);
    RowCost::aur(
        time,
        metrics.stale.contains(&u.name),
        metrics.built_roots.contains(&u.name),
    )
}

/// The trailing `built` tag for an already-built AUR row — green when colored,
/// plain otherwise. Rendered unaligned at the end of the row, like the session
/// badges, so it never perturbs column math.
fn built_tag(colored: bool) -> String {
    if colored {
        style("built").green().to_string()
    } else {
        "built".to_owned()
    }
}

/// A right-justified build-time column padded to `width` visible columns. The
/// pad is measured from the plain cell so a dimmed estimate's ANSI escapes
/// don't skew it. AUR rows fill the column; repo rows ([`RowCost::none`])
/// collapse to blanks that keep it aligned. Shared by the picker and the
/// change-set preview so both lay the column out identically.
pub(super) fn time_col(cost: RowCost, width: usize, colored: bool) -> String {
    let pad = " ".repeat(width.saturating_sub(cost.visible_len()));
    format!("{pad}{}", cost.cell(colored))
}

/// The trailing `  built` tag (with its leading gap) for an already-built row,
/// or empty otherwise. Unaligned — appended after the last aligned column, like
/// the session badges, so it never perturbs column math.
pub(super) fn built_suffix(cost: RowCost, colored: bool) -> String {
    if cost.built {
        format!("  {}", built_tag(colored))
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `should_dim` is the decision behind the dim affordance. Only the exact
    /// combination `(colored, faded=true, Estimate)` qualifies; the other axes
    /// all suppress it.
    #[test]
    fn time_est_should_dim_truth_table() {
        let est = TimeEst::Estimate(60);
        assert!(est.should_dim(true, true));
        assert!(!est.should_dim(false, true), "plain must skip dim");
        assert!(!est.should_dim(true, false), "non-faded must skip dim");
        assert!(
            !TimeEst::Unknown.should_dim(true, true),
            "Unknown must never dim — `~?` dimmed looks like a render glitch",
        );
        assert!(
            !TimeEst::None.should_dim(true, true),
            "None has no cell to dim"
        );
    }

    /// A built `Unknown` row renders an empty time cell (the `built` tag carries
    /// the signal), while a built `Estimate` keeps its number; `visible_len`
    /// tracks the cell actually rendered, not the canonical `render()`.
    #[test]
    fn built_unknown_cell_is_empty() {
        let built_unknown = RowCost::aur(TimeEst::Unknown, false, true);
        assert_eq!(built_unknown.cell(false), "");
        assert_eq!(built_unknown.visible_len(), 0);
        // Not built: the Unknown row still shows `~?`.
        let unknown = RowCost::aur(TimeEst::Unknown, false, false);
        assert_eq!(unknown.cell(false), "~?");
        assert_eq!(unknown.visible_len(), 2);
        // A built estimate keeps its plain text (dimming only adds ANSI, which
        // the plain-paint path skips).
        assert_eq!(
            RowCost::aur(TimeEst::Estimate(60), false, true).cell(false),
            "~1m 0s"
        );
    }

    /// `built_suffix` is the unaligned trailing tag: present iff the row is
    /// built, with its leading gap; the plain form is exactly `  built`.
    #[test]
    fn built_suffix_only_when_built() {
        assert_eq!(
            built_suffix(RowCost::aur(TimeEst::Unknown, false, true), false),
            "  built"
        );
        assert_eq!(
            built_suffix(RowCost::aur(TimeEst::Unknown, false, false), false),
            ""
        );
        assert_eq!(built_suffix(RowCost::none(), false), "");
    }
}
