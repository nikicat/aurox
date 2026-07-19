# TODO

## Shell

- `upgrade` runs the AUR refresh unconditionally whenever the AUR is
  enabled in config: there is no way to upgrade just pacman packages
  without sitting through the AUR fetch first — bad UX on a slow-mirror
  day. The repo half should be reachable without (or before) the AUR half.
  (A ^C mid-refresh now aborts cleanly back to the prompt — see the Done
  note below — so one option is to let it degrade the upgrade to repo-only
  rather than abandoning it entirely.)
- search results should be colored — the shell's numbered list renders as a
  dim monochrome table (`src/ui/search_table.rs`) while `-Ss` styles
  repo/name/version. Whatever palette lands, the installed flag must stay
  clearly visible (today it's row emphasis plus the `old → new` version
  cell, which color alone could drown out).
- search ranking should *weight* freshness, not just tie-break on it. Today the
  order is match-tier → shorter-name → repo-before-AUR → freshest-commit, where
  freshness is only the final AUR tie-break (`src/cli/search.rs` `RankKey`). So a
  stale/abandoned AUR package that matches the name a little better outranks a
  fresh, maintained one — now glaring since the freshness-age risk band renders the
  staleness right at the top of the list (bottom-up = nearest the prompt). Fold
  the freshness *band* (`ui::freshness::FreshnessBand`) into the sort so
  stale/abandoned rows sink and healthy ones rise *within* a match tier; consider
  also demoting the too-fresh *caution* band (recency is non-monotonic — see the
  band model). Name-match quality stays primary; freshness is a secondary weight,
  not an override.
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

<!-- Done:
- config-selectable two-line search rows, pacman-style (`№ repo/name version
  [installed] [age]` headline + indented description line): a typed
  `SearchLayout` knob (`auto`/`single`/`double`, default `auto`) resolved by
  `ui::SearchList`, which renders best-first rows best-last at *row* granularity
  (a two-line row's headline + desc stay paired, unlike the old flat
  `Table::reversed`). `auto` measures the single-line layout against the terminal
  width (`ui::term_width`) and flips to two-line when a row would wrap; a pipe
  (no width) stays dense single-line. `-Ss` stays two-line for pacman parity.
  The two-line renderer + the knob live in `ui/search_layout.rs`; the
  `[installed]`/`[installed: X]` marker text is shared with `-Ss`
  (`installed_marker_text`) so the two can't drift.
- Ctrl-C at the *idle* shell prompt exits aurox (130 = 128+SIGINT), like
  Ctrl-D — mid-operation ^C still bails to the prompt, but an idle ^C now
  means "leave the shell" instead of being swallowed. Demoed by
  examples/demo_ctrlc_quit.rs (a bash-visible `echo $?` shows the 130);
  pinned by extended/38.
- Ctrl-C during the *official-repo* DB refresh aborts the download promptly
  instead of waiting the transfer out: libalpm's internal downloader can't be
  interrupted from outside (pacman _Exits on ^C), so the refresh handle now
  registers aurox's own fetch callback (src/pacman/dload.rs, curl) whose
  progress meter watches the SIGINT flag; `refresh_sync_db` runs under
  `interrupt::cancel_on_sigint` (moved out of mirror.rs), which also stops a
  repo-only refresh from dying to the default SIGINT disposition. Same
  If-Modified-Since/mtime semantics as libalpm's downloader, `file://`
  included. Demoed by examples/demo_ctrlc_repo_refresh.rs (against
  hung_mirror); pinned by extended/39 + smoke/55.
- save review approvals for concrete versions persistently: consented
  approvals (diff answered at the prompt, explicit `approve`) land in
  `reviews.db` keyed by (pkgbase, PKGBUILD commit) — src/build/reviews.rs.
  The pipeline skips re-review at the same commit; the shell stages
  previously-approved versions pre-approved. `--noconfirm` and the unseen
  tail of an "approve all" never persist.
- Ctrl-C during a shell repo/AUR *refresh* now bails back to the prompt instead
  of taking aurox down: `mirror::cancel_on_sigint` wraps the gix fetch/clone in
  a SIGINT guard (the build path's `signal_hook` pattern), and a new
  gix-transport `http::Options::should_interrupt` lets the curl backend abort a
  fetch parked on an idle/slow socket that the cooperative check can't reach.
  Demoed by examples/demo_ctrlc_refresh.rs against examples/hung_mirror.rs (a
  server that answers headers then stalls); pinned by extended/37.
- show time since last commit for AUR packages: the transaction table renders
  a dimmed `(Xd ago)` age cell per AUR row (from the pkgbase's branch-tip
  commit time), and search ranks AUR ties freshest-first.
- remove ~ before times/sizes: the approximate prefix is gone everywhere
  (per-cell + totals + search list); an estimate now reads as the bare figure.
  A *summed* total that under-counts because a row's figure is unknown is a
  lower bound, rendered `>XXhYYm` / `>N MiB` instead. (src/ui/cost.rs +
  src/ui/change_set.rs)
- never-built build-time no longer renders `~0s build`: an all-unknown build
  total is `? build`; TimeEst/SizeEst totals collapse to their own figure kind.
-->

## Related design note

The build-time figure is a real `TimeEst` (`Estimate(Duration)` / `Unknown` /
`None`) and its per-batch *total* is a `TimeTotal` (`Measured{total,bound}` /
`Unknown` / `None`); size mirrors it (`SizeEst` cell, `SizeTotal` total). The
`bound: Bound::{Exact,Lower}` on a total is what prints the `>` lower-bound
marker when an unknown row drags the sum below the true value.
