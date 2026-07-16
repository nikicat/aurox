//! Aligned pacman/yay-style tables for the flag paths: install plans (`-S`)
//! and upgrade plans (`-Qu`/`-Su`). The rendering primitives live in
//! [`super::grid`], the shared verdiff version cell in [`super::cells`].
//! Both renderers *return* their lines — the callers own the stderr framing —
//! so the layout is unit-testable.

use super::cells::{VersionColumn, repo_cell};
use super::dim;
use super::grid::{Cell, Col, Grid, GridRow, Paint, Table};
use crate::names::PkgName;
use crate::pacman::invoke::PkgUpgrade;
use crate::pacman::verdiff;

use console::style;

/// The dimmed-when-colored `Label (N)` header line both flag tables carry.
fn header_line(label: &str, count: usize, paint: Paint) -> String {
    let header = format!("{label} ({count})");
    if paint.colored() {
        dim(&header).to_string()
    } else {
        header
    }
}

/// Render an aligned install plan table:
///
/// ```text
/// Repo packages (explicit) (2)
///     firefox          110.0-1
///     vim              9.1-2
/// ```
///
/// Companion to [`upgrade_table`] for `-S <pkg>` plans — the rows here are
/// always fresh installs (anything already at the target version was dropped
/// by the resolver), so there's no `old -> new` arrow to draw. An empty
/// `version` (e.g. an AUR name we couldn't look up) renders the name alone.
/// Empty `rows` render an empty table (nothing, not a bare header).
pub fn install_table(label: &str, rows: &[(String, String)], paint: Paint) -> Table {
    let mut out = Table::new();
    if rows.is_empty() {
        return out;
    }
    out.push(header_line(label, rows.len(), paint));
    let mut grid = Grid::new(vec![Col::left(), Col::left()]).indent("    ");
    for (name, ver) in rows {
        grid.push(GridRow::new(vec![
            Cell::plain(name.as_str()),
            Cell::paint(ver, paint, |s| style(s).green().to_string()),
        ]));
    }
    out.append(grid.render());
    out
}

/// Render an aligned, colorized upgrade table:
///
/// ```text
/// Upgrades (5)
///     core      glibc            2.40-1        -> 2.41-1
///     extra     neovim           0.10.0-1      -> 0.10.2-1
///     multilib  wine             9.20-1        -> 9.21-1
///     aur       paru-bin         2.0.0-1       -> 2.0.1-1
///     aur       neovim-git       0.10.0.r123-1 -> 0.10.0.r130-1
/// ```
///
/// Rows are grouped by `repo` (canonical Arch order — core → extra →
/// multilib → other → aur), then severity-descending within group. All four
/// columns are space-padded uniformly across the whole list so package names
/// align regardless of which repo they come from. Version cells render via
/// the shared [`VersionColumn`] verdiff cell — common prefix dimmed, the
/// diverging suffix colored by
/// [`BumpKind`](crate::pacman::verdiff::BumpKind) (epoch/major red, minor
/// yellow, patch green, pkgrel cyan) — so this table and the shell's
/// transaction table read identically. An empty `plan` renders an empty
/// table.
pub fn upgrade_table(plan: &[PkgUpgrade], paint: Paint) -> Table {
    let mut out = Table::new();
    if plan.is_empty() {
        return out;
    }
    let ordered = sort_for_display(plan);
    out.push(header_line("Upgrades", ordered.len(), paint));
    let versions =
        VersionColumn::measure(ordered.iter().map(|u| (Some(&u.old_ver), Some(&u.new_ver))));
    let mut grid = Grid::new(vec![Col::left(), Col::left(), Col::left()]).indent("    ");
    for u in &ordered {
        grid.push(GridRow::new(vec![
            repo_cell(&u.repo, paint),
            Cell::plain(u.name.as_str()),
            versions.cell(Some(&u.old_ver), Some(&u.new_ver), paint),
        ]));
    }
    out.append(grid.render());
    out
}

/// The repo half of an `apply`'s upgrade transaction.
///
/// Built by the shell's `apply` (`repo_upgrade_selection`) and consumed by
/// [`crate::cli::dispatch::run_repo_upgrade`]: `repo` is the staged subset,
/// `repo_skipped` becomes the `--ignore=` list for the partial `pacman -Syu`.
/// `aur` is unused on this path (the AUR half goes through the build pipeline),
/// but kept so the type can also describe a full repo+AUR selection.
// No `Eq` — `PkgUpgrade.old_ver` / `new_ver` are `Version`, whose `PartialEq`
// is vercmp (not bytes-equal), and so doesn't satisfy `Eq`'s reflexivity
// guarantee in the bytes-distinct-but-vercmp-equal corner case. `Vec<_>` /
// HashMap usage doesn't rely on `Eq` here.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UpgradeSelection {
    pub repo: Vec<PkgName>,
    pub repo_skipped: Vec<PkgName>,
    pub aur: Vec<PkgUpgrade>,
}

impl UpgradeSelection {
    pub const fn is_empty(&self) -> bool {
        self.repo.is_empty() && self.aur.is_empty()
    }
}

/// Sort `plan` by (repo group, severity-descending, name) without copying.
/// The name tiebreaker keeps the table deterministic across runs — alpm's
/// localdb walk and the `HashMap`-backed foreign-pkg iterator both produce
/// non-stable input order, so a row's position would otherwise jitter
/// between invocations.
pub(super) fn sort_for_display(plan: &[PkgUpgrade]) -> Vec<&PkgUpgrade> {
    let mut rows: Vec<&PkgUpgrade> = plan.iter().collect();
    rows.sort_by(|a, b| {
        a.repo
            .rank()
            .cmp(&b.repo.rank())
            // Group same-rank `Other` repos by their concrete name; a no-op for
            // the canonical repos and AUR (constant name within a rank).
            .then_with(|| a.repo.as_str().cmp(b.repo.as_str()))
            .then_with(|| {
                verdiff::classify_bump(&a.old_ver, &a.new_ver)
                    .cmp(&verdiff::classify_bump(&b.old_ver, &b.new_ver))
            })
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sort_for_display` is the single source of truth for upgrade-row order.
    /// Within one repo it must emit most-severe-first, then alphabetical-by-name
    /// for same-severity rows so the table is deterministic across runs (alpm
    /// and `HashMap` iterators give non-stable input order). Covers both
    /// `upgrade_table` and the picker.
    #[test]
    fn sort_for_display_severity_then_name() {
        // Input is deliberately scrambled — `patch-b` before `patch-a` — so
        // the assertion would fail if the sort fell back to input order.
        let ups = vec![
            PkgUpgrade {
                repo: "extra".into(),
                name: "patch-b".into(),
                old_ver: "2.3.4-1".into(),
                new_ver: "2.3.5-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "major".into(),
                old_ver: "1.0-1".into(),
                new_ver: "2.0-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "pkgrel".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.0-2".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "epoch".into(),
                old_ver: "1:1.0-1".into(),
                new_ver: "2:1.0-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "patch-a".into(),
                old_ver: "1.0.0-1".into(),
                new_ver: "1.0.1-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "minor".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.1-1".into(),
            },
        ];
        let sorted: Vec<&PkgName> = sort_for_display(&ups).iter().map(|u| &u.name).collect();
        assert_eq!(
            sorted,
            ["epoch", "major", "minor", "patch-a", "patch-b", "pkgrel"]
        );
    }

    /// Group ordering: core → extra → multilib → (other repos, alphabetical)
    /// → aur. Severity inside each group still applies.
    #[test]
    fn sort_for_display_groups_then_severity() {
        let ups = vec![
            PkgUpgrade {
                repo: "aur".into(),
                name: "aur-major".into(),
                old_ver: "1.0-1".into(),
                new_ver: "2.0-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "extra-patch".into(),
                old_ver: "1.0.0-1".into(),
                new_ver: "1.0.1-1".into(),
            },
            PkgUpgrade {
                repo: "core".into(),
                name: "core-pkgrel".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.0-2".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "extra-major".into(),
                old_ver: "1.0-1".into(),
                new_ver: "2.0-1".into(),
            },
            PkgUpgrade {
                repo: "multilib".into(),
                name: "ml-minor".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.1-1".into(),
            },
            PkgUpgrade {
                repo: "testing".into(),
                name: "testing-patch".into(),
                old_ver: "1.0.0-1".into(),
                new_ver: "1.0.1-1".into(),
            },
        ];
        let sorted: Vec<&PkgName> = sort_for_display(&ups).iter().map(|u| &u.name).collect();
        assert_eq!(
            sorted,
            [
                "core-pkgrel",
                "extra-major",
                "extra-patch",
                "ml-minor",
                "testing-patch",
                "aur-major",
            ]
        );
    }

    /// The rendered install plan: a dim-able header, 4-space indent, aligned
    /// name column; an empty version cell (a provides-only match) renders the
    /// name alone, and empty rows render nothing at all.
    #[test]
    fn install_table_renders_rows() {
        use crate::{assert_not_contains, assert_regex};
        let rows = vec![
            ("short".to_owned(), "1.0-1".to_owned()),
            ("much-longer-name".to_owned(), "1.2.3-4".to_owned()),
            ("provides-only".to_owned(), String::new()),
        ];
        let table = install_table("Test installs", &rows, Paint::Plain);
        let lines = table.lines();
        assert_eq!(lines[0], "Test installs (3)");
        assert_regex!(lines[1], r"^    short\s+1\.0-1$");
        assert_regex!(lines[2], r"^    much-longer-name  1\.2\.3-4$");
        assert_eq!(lines[3], "    provides-only");
        assert_not_contains!(lines[3], " \n", "no trailing pad on an empty cell");
        assert!(install_table("Empty", &[], Paint::Plain).is_empty());
    }

    /// The rendered upgrade table: header, 4-space indent, repo-grouped rows,
    /// and the shared verdiff version cell (` -> ` in plain paint). Pins the
    /// flag path's layout now that it returns its lines.
    #[test]
    fn upgrade_table_renders_sorted_rows() {
        use crate::assert_regex;
        let ups = vec![
            PkgUpgrade {
                repo: "aur".into(),
                name: "epochpkg".into(),
                old_ver: "1:1.0-1".into(),
                new_ver: "2:1.0-1".into(),
            },
            PkgUpgrade {
                repo: "core".into(),
                name: "short".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.0-2".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "much-longer-name".into(),
                old_ver: "1.2.3-1".into(),
                new_ver: "2.0.0-1".into(),
            },
        ];
        let table = upgrade_table(&ups, Paint::Plain);
        let lines = table.lines();
        assert_eq!(lines[0], "Upgrades (3)");
        // Repo-grouped order (core → extra → aur), names aligned, one-space
        // ` -> ` from the shared version cell.
        assert_regex!(lines[1], r"^    core\s+short\s+1\.0-1\s+-> 1\.0-2$");
        assert_regex!(
            lines[2],
            r"^    extra\s+much-longer-name\s+1\.2\.3-1\s+-> 2\.0\.0-1$"
        );
        assert_regex!(lines[3], r"^    aur\s+epochpkg\s+1:1\.0-1\s+-> 2:1\.0-1$");
        assert!(upgrade_table(&[], Paint::Plain).is_empty());
    }
}
