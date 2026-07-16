//! The verdiff version cell — the one semantic cell constructor shared by
//! every table that renders an `old → new` (or fresh-install) version column.
//!
//! [`VersionColumn`] names the measured `(old, new)` slot widths that used to
//! travel as loose `(old_w, new_w)` pairs; [`version_block`] renders one row's
//! composite block, internally aligned so the grid sees a single fixed-width
//! [`Cell`]. Keeping the arrow-suppression / unknown-version / verdiff-split
//! logic here — behind one constructor — is what lets the transaction, search,
//! and upgrade tables read identically.

use super::dim;
use super::grid::{Cell, Paint, Width};
use crate::pacman::verdiff::{self, BumpKind};
use crate::version::Version;
use console::style;

/// The measured old/new slot widths of a version column, computed once over
/// the rows that feed it.
///
/// Every cell the column renders — upgrade, fresh install, unknown — occupies
/// exactly `old_w + paint.arrow() + new_w` visible columns, so the column
/// after it aligns across all row shapes. This type is the only place
/// [`Paint::arrow`] feeds a width.
pub(super) struct VersionColumn {
    pub(super) old_w: Width,
    pub(super) new_w: Width,
}

impl VersionColumn {
    /// Measure the column over the version pairs of all rows.
    pub(super) fn measure<'a>(
        pairs: impl Iterator<Item = (Option<&'a Version>, Option<&'a Version>)>,
    ) -> Self {
        let mut old_w = Width::ZERO;
        let mut new_w = Width::ZERO;
        for (old, new) in pairs {
            if let Some(v) = old {
                old_w = old_w.max(Width::of(v.as_str()));
            }
            if let Some(v) = new {
                new_w = new_w.max(Width::of(v.as_str()));
            }
        }
        Self { old_w, new_w }
    }

    /// One row's version cell — the composite block as a fixed-width grid cell.
    pub(super) fn cell(&self, old: Option<&Version>, new: Option<&Version>, paint: Paint) -> Cell {
        Cell::sized(
            version_block(old, new, self.old_w, self.new_w, paint),
            self.old_w + paint.arrow() + self.new_w,
        )
    }
}

/// Render one row's version block, padded to a fixed
/// `old_w + paint.arrow() + new_w` visible width so the column after it
/// aligns across install and upgrade rows.
///
/// - **Upgrade** (`old` present): verdiff coloring — common prefix dimmed, the
///   diverging suffix colored by [`BumpKind`], joined by a dimmed ` → `.
/// - **Fresh install** (`old` is `None`): the arrow is suppressed (blank gap)
///   and `new` renders green ("will install").
/// - **Unknown version** (`new` is `None`): an all-blank block of the same
///   width, so a row we couldn't resolve a version for still aligns.
pub(super) fn version_block(
    old: Option<&Version>,
    new: Option<&Version>,
    old_w: Width,
    new_w: Width,
    paint: Paint,
) -> String {
    let Some(new) = new else {
        return (old_w + paint.arrow() + new_w).blanks();
    };
    let new_str = new.as_str();
    let new_pad = new_w.gap(Width::of(new_str));

    let Some(old) = old else {
        // Fresh install: blank old slot + blank arrow gap, then green `new`.
        let lead = (old_w + paint.arrow()).blanks();
        let shown = if paint.colored() {
            style(new_str).green().to_string()
        } else {
            new_str.to_owned()
        };
        return format!("{lead}{shown}{new_pad}");
    };

    let old_pad = old_w.gap(Width::of(old.as_str()));
    if !paint.colored() {
        return format!("{}{old_pad} -> {new_str}{new_pad}", old.as_str());
    }
    let kind = verdiff::classify_bump(old, new);
    let cut = verdiff::common_prefix_at_boundary(old, new);
    let (old_pre, old_suf) = old.as_str().split_at(cut);
    let (new_pre, new_suf) = new_str.split_at(cut);
    format!(
        "{}{}{old_pad}{}{}{}{new_pad}",
        style(old_pre).dim(),
        style(old_suf).red(),
        dim(" → "),
        style(new_pre).dim(),
        paint_suffix(new_suf, kind),
    )
}

/// Color a version's diverging suffix by how severe the bump is.
pub(super) fn paint_suffix(s: &str, kind: BumpKind) -> console::StyledObject<&str> {
    match kind {
        BumpKind::Epoch | BumpKind::Major => style(s).red().bold(),
        BumpKind::Minor => style(s).yellow().bold(),
        BumpKind::Patch => style(s).green(),
        BumpKind::PkgRel => style(s).cyan(),
        BumpKind::Other => style(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(s: &str) -> Version {
        Version::from(s)
    }

    /// `measure` takes the widest old and new over all rows, skipping the
    /// absent slots of fresh-install and unknown-version rows.
    #[test]
    fn measure_takes_widest_slots() {
        let pairs = [
            (Some(ver("1.0-1")), Some(ver("2.0-1"))),
            (None, Some(ver("10.0.0-1"))),
            (Some(ver("1:1.0.0-1")), None),
        ];
        let vc = VersionColumn::measure(pairs.iter().map(|(o, n)| (o.as_ref(), n.as_ref())));
        assert_eq!(vc.old_w, Width::of("1:1.0.0-1"));
        assert_eq!(vc.new_w, Width::of("10.0.0-1"));
    }

    /// Every cell shape occupies the same fixed width in both paints — the
    /// invariant that keeps the column after the versions aligned.
    #[test]
    fn every_cell_shape_has_the_column_width() {
        let old = ver("1.0-1");
        let new = ver("2.0-1");
        let vc = VersionColumn::measure(std::iter::once((Some(&old), Some(&new))));
        for paint in [Paint::Plain, Paint::Colored] {
            let expect = vc.old_w + paint.arrow() + vc.new_w;
            for (o, n) in [
                (Some(&old), Some(&new)), // upgrade
                (None, Some(&new)),       // fresh install
                (Some(&old), None),       // unknown target version
            ] {
                assert_eq!(
                    vc.cell(o, n, paint).width(),
                    expect,
                    "({o:?} -> {n:?}) under {paint:?}"
                );
            }
        }
    }

    #[test]
    fn paint_suffix_dispatches_every_kind() {
        // Smoke-test the dispatch table: every BumpKind renders a string that
        // still contains the input text. Exact ANSI codes are an internal of
        // `console` and not worth pinning.
        for kind in [
            BumpKind::Epoch,
            BumpKind::Major,
            BumpKind::Minor,
            BumpKind::Patch,
            BumpKind::PkgRel,
            BumpKind::Other,
        ] {
            let s = paint_suffix("1.2.3", kind).force_styling(true).to_string();
            assert!(s.contains("1.2.3"), "{kind:?} dropped the text: {s:?}");
        }
    }

    /// The plain upgrade cell reads `old -> new`; the fresh cell has no arrow.
    #[test]
    fn plain_shapes() {
        let old = ver("1.0-1");
        let new = ver("1.1-1");
        let vc = VersionColumn::measure(std::iter::once((Some(&old), Some(&new))));
        let up = version_block(Some(&old), Some(&new), vc.old_w, vc.new_w, Paint::Plain);
        assert_eq!(up, "1.0-1 -> 1.1-1");
        let fresh = version_block(None, Some(&new), vc.old_w, vc.new_w, Paint::Plain);
        assert!(
            !fresh.contains("->"),
            "fresh install has no arrow: {fresh:?}"
        );
        assert!(fresh.trim_start().starts_with("1.1-1"), "{fresh:?}");
    }
}
