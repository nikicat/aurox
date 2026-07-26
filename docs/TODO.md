# TODO

Open work only. A finished item is *deleted*, not archived here: the commit
carries what changed and why, and any rationale worth keeping belongs next to
the code it constrains (a module doc, CLAUDE.md's conventions, docs/TESTING.md)
— never in a list nobody reads on the way to the answer.

## Core

- **one source type, not five spellings.** "Repo or AUR?" is asked in at
  least five different vocabularies today: `cart::Source` (`Repo`/`Aur`),
  `build::SourcePin` (the same pair as a routing pin), the `RepoName ==
  REPO_AUR` sentinel-string test (`CartItem::from_upgrade`,
  `staging::stage_class_from_pick`), `RepoName::rank() == RepoRank::Aur`
  (`ui/cost.rs`, `shell/upgrade.rs`), and `search::Row` (`Repo`/`Aur`) — plus
  `StageClass { source, repo: Option<RepoName> }`, where the `None` encodes
  "AUR" a *second* time alongside the `source` field that already said so.
  Each one bottoms out in a `match { Repo => …, Aur => … }`, ~160 sites
  crate-wide (concentrated in `cli/shell/{staging,verbs,cart}.rs`), and every
  new surface adds another arm pair that can drift from its siblings.
  Wanted: **one** type that answers where a package comes from, carrying the
  concrete sync-DB *inside* the repo case (`Source::Repo(RepoName)` /
  `Source::Aur`, or a `RepoName` that knows it may be the AUR) so the coarse
  lane and the concrete repo stop travelling as two fields that can disagree;
  then move the per-lane behaviour onto that type (`label`, default approval,
  version/size lookup, install routing) so call sites read as one path.
  **Watch:** some differences are real — an AUR row has no syncdb version and
  no download size, a repo row never builds — so the goal is fewer matches,
  not a uniform interface that answers `None`/`0` on half its calls. The
  "absent provider = empty provider" convention already did this for
  *availability*; this is the same move for *identity*.

## Shell

- `upgrade` runs the AUR refresh unconditionally whenever the AUR is
  enabled in config: there is no way to upgrade just pacman packages
  without sitting through the AUR fetch first — bad UX on a slow-mirror
  day. The repo half should be reachable without (or before) the AUR half.
  (A ^C mid-refresh already aborts cleanly back to the prompt, so one option
  is to let that degrade the upgrade to repo-only rather than abandoning it
  entirely.)
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

*Both postponed (2026-07-26) — a priority call, not a blocker: the seven
demos already in `demos/demos.json` cover the flows a newcomer needs, and the
product items above come first.*

- initial AUR mirror clone, sped up: the one-time ~2 GiB clone with its
  progress UI, time-compressed to ~15 s. The mock mirror clones instantly
  (70 one-commit branches — no byte stream, no ref-writing tail), so the
  obvious path is a hand-recorded real clone whose cast timestamps are
  rescaled (asciicast times are trivially editable), with the `.cast` checked
  in as the source so the GIF still renders reproducibly. **But check the
  cheaper option first:** the two things worth filming — the 155k figure and
  the long silent "finalizing — writing refs" phase with its idle note
  (`ui/gix_progress.rs`) — are driven by *ref count*, not payload, and refs
  are cheap. A synthetic mirror of 155k one-commit branches via `git
  fast-import` should reproduce both hermetically for tens of MB of image and
  a minute or two of bake (unmeasured — that's the thing to check). Only the
  byte counter would understate. If it holds, this is a normal hermetic
  recording like the other seven, repeatable in CI, and the non-hermetic
  hand-recording is unnecessary.
- incremental refresh: `-Sy` after a branch moves on the mirror — reuse
  extended/18's hermetic bump mechanics (clone the mock-AUR branch, commit
  a pkgver bump, fetch it back) to show "no ref updates" vs
  "1 ref(s) updated" + the index catching the new version.

## AUR

- **an AUR row's download figure should be its `source=` files, minus what's
  already fetched** — *postponed until deeper PKGBUILD analysis lands; the
  note is here so the intent isn't re-derived from scratch.* Today the 📥
  column tells the truth only for repo rows (`sync_download_size` is alpm's
  `download_size()`, already 0 for a cached archive); an AUR row instead
  shows `SizeEst::Estimate(installed_size)` — its *installed footprint*, a
  different quantity summed into the same total (`ui/cost.rs` `size_of`).
  What it should be is the bytes makepkg will actually fetch: the `source=`
  entries not already in the build worktree / `SRCDEST`.
  **Why it waits:** nothing in the index knows a pkgbase's sources —
  [`IndexEntry`](../src/index/schema.rs) has no `source` field and
  `index::srcinfo` drops those lines — and even with them, a size needs a
  HEAD request per http source and a non-trivial fetch to size a `git+`
  one, per row, at *preview* time. That's a network round-trip storm and a
  new failure mode for an advisory number. So it rides along with whatever
  brings real source analysis into the index, not before.
