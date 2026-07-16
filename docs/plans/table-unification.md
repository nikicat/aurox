# Plan: one column-layout engine for all tables

Status: **implemented** (all seven steps). This doc records the design and the
extensibility seams it deliberately leaves open.

## Why

Nine sites each re-implemented aligned-column terminal output, and they had
already drifted: the change-set table right-aligned its size column while the
search table left-aligned it; the system report used a 1-space gutter where
everything else used 2; `tables::render_row` duplicated `version_block`'s
verdiff rendering with bare-`usize` widths; `install_table` padded by byte
length; the shell's search list got its row numbers bolted on in a second
`format!` pass; and `flat_cart_lines` hand-rolled the show-table's column set
a second time. The TODO that named this lived at the `system show` renderer:
*"One shared column-layout helper should own padding/alignment so the
conventions can't drift per call site."*

## The three layers

1. **Row models** — per-surface typed rows (`TxnRoot`, `SearchRow`,
   `PkgUpgrade`, `system::Report`, `ListItem`), pure data, renderer-free. No
   unified über-row: it would be `Option` soup, and the surfaces genuinely
   differ. **Invariant: rows never store rendered text** (the shell's
   `ListItem` lost its `label` field to restore this).
2. **Cell vocabulary** (`ui/cells.rs`, `ui/cost.rs`) — one constructor per
   visual concept: `VersionColumn` (the measured old/new widths + the verdiff
   `old → new` composite cell; the only place `Paint::arrow` feeds a width),
   `repo_cell` (yay's hashed repo color), the size/build-time cells. Every
   constructor takes `Paint` explicitly — **no ambient `color_on()` reads in
   renderers**; styling enters only through `Cell::paint` closures.
3. **Layout engine** (`ui/grid.rs`) — `Grid` over `Col` specs
   (`left()`/`right()`, `.min(width)` floor) and `GridRow`s (cells + an
   unaligned `tail` for the `built`/age/description appendices). The engine
   owns what call sites may no longer re-implement: pad by *visible* width
   (`Cell` carries it, so ANSI never skews columns), 2-space gutter,
   no trailing whitespace, per-line `indent`.

Complex tables are surface-owned **compositions**: grids for aligned sections
plus literal `Table::push` lines (section markers, totals, removal rows).
Columns shared *across* sections — the change-set's size/time columns spanning
roots and the dep block — are measured once over the union and handed to each
grid as a `Col::min` floor.

Emission is unified too: `Table::eprint_framed()` is the flag-path stderr
frame; `ShellEnv::print_table` is the one place a table meets the shell's
stdout seam. Rendering that needs I/O-shaped context (live pacman DBs, paint)
stays on the env side of the seam — `RealEnv::search` prints its own numbered
table (worst-first via `Table::reversed()`), exactly like `render_cart`;
dispatch only words data decisions.

## What renders through the grid

| surface | columns | file |
| --- | --- | --- |
| shell `show` (roots) | `№ R · repo · approval · name · version · size R · time R` + built/age tail | `ui/change_set.rs` |
| shell `show` (dep block) | `name · tag(min "(install)") · size R(min) · time R(min)` + built tail, indent 8 | `ui/change_set.rs` |
| search (shell / pipe) | `[№ R(min 3)] · repo · name · version · size · time R` + desc tail (`RowNumbers`) | `ui/search_table.rs` |
| `-Qu`/`-Su` upgrades | `repo · name · version` (header line above) | `ui/tables.rs` |
| `-S` plan groups | `name · version` (header line above) | `ui/tables.rs` |
| `system show` | `label · size R` + desc/[cache] tail, indent 2 | `cli/shell/verbs.rs` |
| show fallback (resolve failed) | `№ R(min 3) · repo · approval · spec` + version tail | `cli/shell/env.rs` |

Deliberate output changes shipped with the port: `-Qu`'s separator normalized
from `  ->  ` to the shared ` -> ` / dimmed ` → ` convention (the point of
killing `render_row`); the system report's label column is measured, gutter
1→2; the flat fallback actually aligns now; no line carries trailing
whitespace.

## Extensibility seams (deliberately not built)

- **Customizable column sets** — a surface's `Vec<Col>` + per-row `Vec<Cell>`
  are plain values built in one function; a future config filters both by a
  `ColumnId` enum introduced *then*.
- **Two-line-per-item layouts** (pacman `VerbosePkgLists` / yay style) — an
  alternate render fn over the same `GridRow`s; rows and cells don't change.
- **External renderings (HTML/JSON/browser)** — consume the Layer-1 row
  models, never rendered tables; the invariant to hold is just "rows never
  store rendered text". A semantic-span `Cell` (style tags instead of ANSI)
  is deferred until a non-ANSI renderer exists — safe because styling is
  contained in the Layer-2 constructors behind `Paint`.
- **TODO.md's age + download-aware size columns** — each is now one cell
  constructor + one `Col` + one cell per row.

Out of scope, still hand-aligned on purpose: `HELP_TEXT` (a static literal;
the right fix is a schema change — per-verb `(usage, summary)` in `TOPICS`
with a generated `help_text()` over `Grid` — a separate change), and the
`-Si` info block + `-Ss` `repo/name` listing (pacman-parity fixed formats;
`{label:<16}: ` is a protocol constant, not a measured column).

## Test anchors

`ui::grid` tests pin the engine conventions; `transaction_table_colored_strips_to_plain`
and `transaction_table_size_column_aligns_across_rows` passed **unmodified**
across the port (the acceptance gate); the flag tables gained their first
content tests when they started returning `Table`; container smoke 52 pins the
upgrade table's exact ANSI repo prefix; the shell PTY e2e set covers the wiring.
