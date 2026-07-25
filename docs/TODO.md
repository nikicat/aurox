# TODO

Open work only. A finished item is *deleted*, not archived here: the commit
carries what changed and why, and any rationale worth keeping belongs next to
the code it constrains (a module doc, CLAUDE.md's conventions, docs/TESTING.md)
— never in a list nobody reads on the way to the answer.

## Shell

- `upgrade` runs the AUR refresh unconditionally whenever the AUR is
  enabled in config: there is no way to upgrade just pacman packages
  without sitting through the AUR fetch first — bad UX on a slow-mirror
  day. The repo half should be reachable without (or before) the AUR half.
  (A ^C mid-refresh already aborts cleanly back to the prompt, so one option
  is to let that degrade the upgrade to repo-only rather than abandoning it
  entirely.)
- search results should be colored — the shell's numbered list renders as a
  dim monochrome table (`src/ui/search_table.rs`) while `-Ss` styles
  repo/name/version. Whatever palette lands, the installed flag must stay
  clearly visible (today it's row emphasis plus the `old → new` version
  cell, which color alone could drown out).
- search ranking still isn't optimal — **research it and rethink from scratch**.
  Today's formula (`src/cli/search.rs` `RankKey`: exact-name → match-tier →
  health → repo>AUR → shorter-name → freshest-commit → lexical) is a reasonable
  hand-tuned heuristic, but it's a lexicographic key *stack* accreted one
  tie-break at a time, not a model grounded in what actually predicts the row a
  user wants. Study the alternatives properly before iterating further: how
  pacman/yay/paru weight relevance; whether a *scored* model (weighted signals
  summed, with a learned/tuned weighting) beats strict tiers; how name-match
  quality, provenance (repo vs AUR), freshness/health, popularity we don't have,
  and installed-state should really trade off; and whether the bottom-up
  "best nearest the prompt" order interacts badly with any of it. Come back with
  a from-first-principles design, not another appended tie-break.
- renderer-agnostic table model (so a **web-UI table renderer** can attach).
  Today the whole grid stack is a *terminal-string* engine: `ui::Cell` stores
  an already-ANSI-baked `String` (via the `Cell::paint(plain, paint, f)`
  closure), and `Grid::render` emits `Table = Vec<String>`. Nothing structured
  survives, so a non-terminal renderer (web, GUI) can consume none of it. The
  fix is **style-as-data**: `Cell { content, style: Style }` where `Style` is a
  data enum (`Dim`, `Bold`, `RepoHash`, `Band(FreshnessBand)`, `VersionDiff{…}`,
  …), the grid emits a *structured* `Table` (rows of styled cells with computed
  widths), and a `TerminalRenderer`/`WebRenderer` each translate `Style` → ANSI
  / CSS. Cross-cutting: touches `ui/grid.rs` + every table renderer
  (`search_table`, `change_set`, `tables`, `cost`, `cells`) + the `ShellEnv`
  print seam. Groundwork already landed: `GridRow.tail` is a structured
  `Vec<Cell>` the grid composes (call sites hand semantic segments, no
  `format!("{}{}")` tails) — so the tail is ready for `Style`-carrying cells;
  the remaining work is making `Cell` itself carry style-as-data instead of a
  rendered string.
- colorize the `info <pkg>` table — it renders monochrome while the
  transaction/search surfaces are styled. Same palette question as the search
  list above; `src/ui/tables.rs` (`install_table`) is the renderer.
- **dropping from an upgrade cart should mark, not delete.** `drop` today
  removes the row (`Cart::unstage`, `src/cli/shell/cart.rs`), so the numbered
  list renumbers under the user and every subsequent selector means something
  different than it did a line ago. A dropped row should stay in place, marked
  *dropped*/skipped and excluded from apply — stable numbering, visible
  decision, trivially undoable by re-adding. Touches the cart model (a per-item
  state, not a `Vec` removal), the change-set renderer, and apply's filter.
- **bug: adding an already-installed package stages it as new, not an
  upgrade.** Observed after `add <installed-pkg>`: the cart row shows the
  install shape instead of `old → new`. The upgrade path gets this right, so
  the installed-version lookup is missing (or ignored) on the `add` staging
  path — find the one seam both should route through.
- bare-token shortcut after a search: entering just a number (`1`, `22`) at the
  prompt should mean `add <number>` against the last search list, and a bare
  package name should mean `add <name>`. Watch the ambiguity with verbs and
  with the selector vocabulary — a bare token that parses as a known verb stays
  a verb.
- rename `apply` → `do`, demoting `apply` to an alias: `do` is what the action
  is, and it's already accepted (`ALIASES` in `src/cli/shell/command.rs` maps
  both `do` and `commit`). Swap the canonical name (`Verb::name`, help text,
  completion, prompts and docs that say "apply") and keep `apply` working.
- noticeable delay on exit: quitting takes a visible beat before the
  terminal prompt returns. Not reproducible at fixture scale — the hero
  demo cast measures quit → bash prompt at ~10 ms — so profile against a
  real-sized state (~2 GiB mirror, 155k-package index): dropping the
  zero-copy index mmaps, gix teardown, and the tracing file-log flush are
  the first suspects.
- **organize command concerns around commands, not around concerns.** A
  command's intrinsic traits are today scattered by *kind* across files: its
  parse arm + sub-action vocab (`command.rs`), its dispatch handler
  (`verbs.rs`, or `staging.rs` for cart verbs), its help one-liner + topic
  (`help.rs` `HELP_TEXT`/`TOPICS`), its completion scope + any bespoke
  arg-slot logic (`complete.rs` `arg_kind` + helpers), and its env-seam
  methods (`ShellEnv` in `shell.rs` + `env.rs`/`testenv.rs`). Adding one
  command is a shotgun edit across all of them, and each concern-file grows
  without bound as commands accumulate — the recent `config` verb touched
  every one of these (`command.rs`, `verbs.rs`, `help.rs`, `complete.rs`,
  `shell.rs`, `env.rs`, `testenv.rs`, plus its own two-slot completion
  special-case). Think about inverting the axis: a per-command descriptor (a
  `ShellCommand` trait, or a const table of command specs) where each command
  declares its name/aliases, parser, dispatch, help text, and completion
  scope in *one* place, and the concern-files become thin drivers that fold
  over that registry. **Constraint to preserve:** today's virtue is
  compile-time completeness — the `Verb` enum is the single source of truth
  and exhaustive `match`es on it (`name`, `arg_kind`, dispatch) plus the
  `every_verb_has_a_help_topic` test mean the *compiler*, not a drift test,
  walks each new verb through every decision (see `Verb`'s doc in
  `command.rs`). Any reorg must keep that — a required-method trait gives it
  for free (a new command can't compile until it supplies parse + dispatch +
  help + completion), which is arguably a strict improvement over the current
  side-table-per-concern arrangement. Watch the seams that *don't* fit a tidy
  per-command box: shared vocab (aliases, `REFRESH_SCOPES`, `CONFIG_ACTIONS`),
  the selector/referent machinery cart verbs share, and multi-slot arg
  completion (`config`'s action-then-path) — a good design accommodates these
  without forcing every command through the widest command's shape.

## Demos (docs/plans/screencasts.md)

- initial AUR mirror clone, sped up: the one-time ~2 GiB clone with its
  progress UI, time-compressed to ~15 s. The mock mirror clones instantly
  (nothing to show) and a live recording is non-hermetic — the pragmatic
  path is a hand-recorded real clone whose cast timestamps are rescaled
  (asciicast times are trivially editable), with the `.cast` checked in as
  the source so the GIF still renders reproducibly.
- incremental refresh: `-Sy` after a branch moves on the mirror — reuse
  extended/18's hermetic bump mechanics (clone the mock-AUR branch, commit
  a pkgver bump, fetch it back) to show "no ref updates" vs
  "1 ref(s) updated" + the index catching the new version.

## AUR

- account for already downloaded sources when printing download sizes in tables
