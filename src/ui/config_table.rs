//! The `config show` table: one row per knob with its **current** and
//! **default** value in separate columns, the current value highlighted when it
//! differs from the default so a customized setting stands out at a glance.
//!
//! Presentation only — the values arrive pre-rendered as [`ConfigRow`]s from
//! [`config::edit`](crate::config::edit); this module owns the columns and the
//! color. Like every renderer here it takes a [`Paint`] (so tests pin
//! [`Paint::Plain`] and the width math measures the *plain* text), and returns a
//! [`Table`] the shell prints through its output seam.

use super::grid::{Cell, Col, Grid, GridRow, Paint, Table};
use console::style;

/// One knob's row in the `config show` table: its dotted path and the two
/// rendered values the table compares.
///
/// Plain data (no color) — the renderer decides how to paint it from whether
/// `current` differs from `default`.
#[derive(Debug)]
pub struct ConfigRow {
    /// The dotted knob path (`color`, `ages.caution_days`).
    pub path: String,
    /// The effective value in force now (a user override, or the default).
    pub current: String,
    /// The built-in default the knob falls back to when unset.
    pub default: String,
}

impl ConfigRow {
    /// Whether the current value differs from the default — the knob the user
    /// has customized, which the table highlights.
    fn changed(&self) -> bool {
        self.current != self.default
    }
}

/// Render the knob rows into the aligned `setting / current / default` table.
///
/// A changed knob's current value is bold+cyan; every default is dimmed so the
/// eye lands on the live column, and unchanged rows read as quiet reference.
pub fn config_table(rows: &[ConfigRow], paint: Paint) -> Table {
    let mut grid = Grid::new(vec![Col::left(), Col::left(), Col::left()]).indent("  ");
    grid.push(GridRow::new(vec![
        header_cell("setting", paint),
        header_cell("current", paint),
        header_cell("default", paint),
    ]));
    for row in rows {
        let current = if row.changed() {
            Cell::paint(&row.current, paint, |s| {
                style(s).cyan().bold().force_styling(true).to_string()
            })
        } else {
            Cell::plain(&row.current)
        };
        let default = Cell::paint(&row.default, paint, |s| {
            style(s).dim().force_styling(true).to_string()
        });
        grid.push(GridRow::new(vec![Cell::plain(&row.path), current, default]));
    }
    grid.render()
}

/// A dimmed column header cell — subtle so it labels without competing with the
/// data below it.
fn header_cell(text: &str, paint: Paint) -> Cell {
    Cell::paint(text, paint, |s| {
        style(s).dim().bold().force_styling(true).to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, current: &str, default: &str) -> ConfigRow {
        ConfigRow {
            path: path.to_owned(),
            current: current.to_owned(),
            default: default.to_owned(),
        }
    }

    /// Plain paint: the columns align (`setting / current / default`) with a
    /// header, and no ANSI escapes leak in — the layout tests every renderer has.
    #[test]
    fn plain_lays_out_setting_current_default_columns() {
        let rows = [row("color", "never", "auto"), row("aur", "true", "true")];
        let table = config_table(&rows, Paint::Plain);
        let lines = table.lines();
        assert_eq!(lines[0], "  setting  current  default");
        assert_eq!(lines[1], "  color    never    auto");
        assert_eq!(lines[2], "  aur      true     true");
        assert!(
            lines.iter().all(|l| l == &console::strip_ansi_codes(l)),
            "plain paint emits no escapes: {lines:?}"
        );
    }

    /// Colored paint highlights only the *changed* knob's current value, and
    /// aligns by plain width regardless of the escapes (the grid's contract).
    #[test]
    fn colored_highlights_only_changed_current_values() {
        let rows = [row("color", "never", "auto"), row("aur", "true", "true")];
        let table = config_table(&rows, Paint::Colored);
        let lines = table.lines();
        // The changed knob's current value carries color; the unchanged one does
        // not (its current cell is plain).
        let changed = &lines[1];
        assert!(
            changed.contains("never") && changed.contains('\u{1b}'),
            "changed current value is colored: {changed:?}"
        );
        // Stripping escapes recovers the same aligned layout as the plain form.
        let stripped: Vec<String> = lines
            .iter()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect();
        assert_eq!(stripped[1], "  color    never    auto");
        assert_eq!(stripped[2], "  aur      true     true");
    }
}
