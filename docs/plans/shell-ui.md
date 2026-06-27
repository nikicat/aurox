# Plan: shell-like (REPL) UI for interactive `gaur`

Status: proposed (not yet implemented)

## Goal

Replace the **wizard-like** interactive UX — fixed linear sequences of modal
`dialoguer` steps (fullscreen `MultiSelect` picker → `Confirm` gate → per-PKGBUILD
review loop) — with a **shell-like** REPL: a persistent prompt where the user
drives with typed word-commands against long-lived session state and a staged
transaction, instead of being walked through prompts in a fixed order.

```
$ gaur
:: refreshing AUR mirror + index … done (3.8s)
:: 14 upgrades available (11 repo, 3 AUR).  type `upgrade` to stage, `help` for commands.
gaur> search yubikey
  1  aur/yubico-authenticator-bin  7.1.1-1   Yubico Authenticator (prebuilt)
  2  aur/yubikey-manager-qt        1.2.5-3   cross-platform GUI …
  3  extra/yubikey-manager         5.5.1-1   [installed] python library + CLI
gaur> add 1 yubikey-personalization*
  + yubico-authenticator-bin        (aur)
  + yubikey-personalization         (aur)
  + yubikey-personalization-gui     (aur)
gaur> upgrade firefox* glibc
  + firefox            115.0-1 → 116.0-1   (extra)
  + firefox-developer  …                   (aur)
  + glibc              2.40-1  → 2.41-1    (core)
gaur> review yubico-authenticator-bin
  … PKGBUILD diff … approve? [y/N] y
  ✓ marked reviewed
gaur> show
:: transaction — 6 package(s), +4 dependencies
    … change-set table with sizes + build-time + total …
gaur> apply
:: Proceed? [Y/n] y
    … build + install, one sudo batch …
gaur> quit
```

## Locked decisions (from review)

1. **Augment, don't replace, the flag CLI.** Bare interactive `gaur` opens the
   shell. Explicit `gaur -S…/-Ss…/-Si…/-Syu`, bare-term search, and all pacman
   pass-through keep their **current one-shot, scriptable** behavior unchanged.
   Non-interactive bare `gaur` (pipe / cron / `--noconfirm`) still does a single
   `-Syu` pass. The shell is *strictly* the interactive no-arg path.
2. **Words-only command vocabulary.** `search` `info` `add` `drop` `remove`
   `upgrade` `review` `show` `apply` `clear` `refresh` `help` `quit`. No
   pacman-letter clusters inside the shell, no clap.
3. **Staged transaction (cart) + explicit `apply`.** `add`/`drop`/`upgrade`
   accumulate a pending set across many commands; `show` previews the *resolved*
   change-set with cost; `apply` resolves → builds → installs **once**. This
   unifies fresh-install and upgrade into a single transaction model and
   subsumes today's `upgrade_loop`.
4. **Selection by numbers *and* package names with wildcards.** Commands that
   produce a list (`search`, `upgrade`, `show`) remember it; selector arguments
   accept numbers (`3`), ranges (`5-8`), exact names (`glibc`), and globs
   (`python-*`, `firefox*`). Numbers/ranges index the last list; names/globs
   resolve against the AUR index + sync DBs.
5. **`apply` is one atomic add+remove transaction** (target state) — a single
   native libalpm transaction carrying repo adds, AUR file adds, *and* removals,
   so "package(group) X replaces package(group) Y" lands without a window where
   neither is installed. See [Applying the transaction](#applying-the-transaction-one-atomic-addremove)
   for why pacman's CLI can't do this and how the native path gets there. Phased:
   the first cut uses pacman calls (which already make *declared* replaces atomic);
   the native combined commit follows.
6. **Tab completion is first-class**, not polish: context-aware completion of
   command verbs and package names from word one. See [Tab completion](#tab-completion).

## Why this fits the codebase

The no-arg upgrade path (`src/cli/upgrade_loop.rs`) is *already* a loop that
hoists the expensive once-per-session work (mirror fetch, index+secondary load,
`MirrorRepo`, metrics store) out of the iteration and only re-snapshots the
localdb per pass (`UpgradeSession` + `recompute_remaining`). The shell is the
**generalization** of that loop: same session hoist, but the fixed
recompute→pick→confirm→apply sequence becomes a command dispatch loop. Most of
the existing machinery is reused as-is; the wizard widgets are what go away.

| Reused unchanged | Becomes shell-native |
| --- | --- |
| `UpgradeSession` (load, `recompute_remaining`, `pkgbase_of`, `index`, `secondary`) | `dialoguer::MultiSelect` picker (`ui::select_upgrades`, `search::pick`) |
| `build::resolve_targets` / `apply_plan` / `cmd_install` (the apply engine) | the fixed confirm-then-review wizard order |
| `ui::change_set_table` + `PreviewMetrics` (the `show`/`apply` preview) | `cli/upgrade_loop.rs` `drive` loop (retired once shell reaches parity) |
| `build::review::review` + `reviewed: HashSet<PkgBase>` (gates `apply`) | |
| `mirror::cmd_refresh`, `alpm_db::open`/`open_synced`, metrics store | |
| `Error::Interrupted` / `Error::UserAbort`, makepkg SIGINT bail-to-table | |

Three current dead-ends dissolve for free under this model:

- **UPDATE_LOOP phase 4** (live inline dep-expansion + `v` review hotkey) was
  blocked because `dialoguer::MultiSelect` has no toggle-time or custom-key hook.
  In the shell, `show` *is* the live dep-expansion (re-resolve + print on demand)
  and `review <pkg>` *is* the `v` hotkey — both plain commands, no custom picker.
- The **route-2 per-root dep nesting** the change-set doc deferred is now just a
  rendering choice we fully own.
- The "**no in-loop refresh**" constraint relaxes into an explicit `refresh`
  command (user opt-in, never automatic mid-transaction).

## Custom types

### `Cart` — the staged transaction

```rust
/// The pending transaction the shell builds up; applied atomically by `apply`.
/// Nothing is persisted — quitting drops it (matches the session-only stance of
/// `upgrade_loop::SessionState`).
struct Cart {
    /// Install/upgrade targets, carrying the counterpart hint through
    /// expand → resolve → prepare exactly like `upgrade_loop::resolve_aur`.
    install: Vec<build::Target>,
    /// Packages staged for removal → `pacman -R` at apply time.
    remove: Vec<PkgName>,
    /// PKGBUILDs approved this session — suppresses repeat review on `apply`.
    reviewed: HashSet<PkgBase>,
    /// Cross-batch badges (failed/interrupted/skipped) carried over retries,
    /// lifted verbatim from `upgrade_loop::SessionState`.
    history: SessionState,
}
```

`apply` consumes the cart and folds the `RunReport` back into `history` so failed
items stay staged and badged for retry (the existing fold logic moves over
intact). *How* the privileged step runs — separate pacman calls vs. one native
libalpm transaction — is the subject of the next section.

### `Selector` — numbers + names + globs

```rust
/// One selector argument. `add`, `drop`, `review`, `info` all parse their args
/// into these against the current displayed list + the index.
enum Selector {
    Index(usize),                 // `3`        → current list row
    Range(usize, usize),          // `5-8`      → current list rows
    Name(PkgTarget),              // `glibc`    → resolve via index/sync
    Glob(globset::GlobMatcher),   // `python-*` → match names in index/sync
}
```

Resolution is a pure function `resolve(selectors, current_list, idx, sec, pac)
-> Vec<PkgTarget>` — the single reusable core, unit-tested without I/O. Numbers
and ranges only ever index the current list (error if out of range); names/globs
resolve against the AUR index + sync DBs (so `add python-*` works with no prior
`search`). A glob that matches nothing warns rather than erroring (shell-like).

### `Command` — the parsed verb

A small enum (`Search(Vec<String>)`, `Add(Vec<Selector>)`, `Upgrade(Vec<Selector>)`,
`Review(Selector)`, `Show`, `Apply`, `Clear`, `Refresh`, `Help(Option<String>)`,
`Quit`, …). Parsing is `shell-words` tokenization + a verb match — no clap.
Unknown verb → a "did you mean / type `help`" note, never an exit.

## Dependencies

- **`rustyline`** for the line editor: history (`$XDG_STATE_HOME/gitaur/shell_history`
  via the existing `paths` helpers), emacs keybindings, and a `Completer` that
  tab-completes verbs + package names straight from the mmap'd index (a genuinely
  nice feature we get cheap because the index is already loaded). Chosen over
  `reedline` (nushell's) as the lighter, battle-tested fit for the existing
  console/dialoguer stack; `reedline` is the fallback if multiline/rich-syntax
  needs ever appear. *Open: confirm rustyline at implementation start.*
- **`shell-words`** for tokenizing the input line (quoting/escapes).
- **`globset`** (or `glob`) for the wildcard selector. *Open: `globset` is
  already common in the Rust ecosystem and fast; confirm it isn't already a
  transitive dep we can lean on.*

rustyline owns the terminal only while reading a line; during `apply` we're away
from the prompt, so `indicatif` bars and the existing review prompt work exactly
as today — no concurrency between the two.

## Module layout

New `src/cli/shell.rs` (parent) + `src/cli/shell/`:

```
src/cli/shell.rs            run(): session hoist + REPL loop + rustyline wiring
src/cli/shell/command.rs    Command enum + parse() (shell-words → verb)
src/cli/shell/selector.rs   Selector enum + resolve() (numbers/ranges/names/globs)
src/cli/shell/cart.rs       Cart + apply() (drives resolve_targets/apply_plan/-R)
src/cli/shell/exec.rs       per-command handlers (search/info/add/show/review/…)
src/cli/shell/complete.rs   rustyline Completer over verbs + index names
```

The control flow is split exactly like `upgrade_loop`'s `drive`/`LoopEnv`: a pure
`dispatch(cmd, &mut state, &mut env) -> Result<Flow>` core behind a `ShellEnv`
trait, so command sequencing (cart mutation, fold, exit conditions) is
unit-testable with a scripted fake env — no mirror, picker, or build.

## Wiring point

`src/cli/dispatch.rs::dispatch`, the existing interactive no-arg branch:

```rust
if f.op.is_none() && f.positional.is_empty() {
    let interactive = !cli.noconfirm && std::io::stdin().is_terminal();
    if interactive {
        return shell::run(cfg, cli.devel || cfg.devel);   // was upgrade_loop::run
    }
    // non-interactive: unchanged single-shot -Syu
}
```

Everything else in `dispatch` and all of `cli::run`'s pre-scan stays untouched —
that is the "augment, keep flags" decision in one line. `upgrade_loop.rs` stays
in place until the shell reaches upgrade parity (phase 3), then is deleted.

## Startup behavior (preserve muscle memory)

Bare `gaur` today means "upgrade". To avoid surprising that habit, the shell on
entry: `mirror::cmd_refresh` once → compute remaining upgrades → print the count
+ a one-line hint, **without auto-staging**. The user types `upgrade` (stage all)
or `upgrade <glob>` (stage some) then `apply` — two tokens to reproduce the old
flow, but now inside an adjustable session. *Open: should entry auto-stage all
upgrades (closer to old behavior) or just list them (safer)? Leaning list-only.*

## Signals

| Ctrl+C arrives during | Result |
| --- | --- |
| line editing at the prompt | rustyline returns `Interrupted`; clear the line, redraw prompt — **never exit** |
| an `apply` build (`makepkg`) | existing `Error::Interrupted` bail: mark pkgbase interrupted, fold, **return to prompt** |
| Ctrl+D (EOF) at the prompt, or `quit`/`exit` | exit the shell cleanly (`Ok(0)`) |

This is the same interrupt contract the loop already implements, with "the table"
renamed to "the prompt".

## Applying the transaction: one atomic add+remove

The motivating case: one package (or package *group*) replaces another, and we
don't want a window where the old set is gone but the new set isn't in yet — and
ideally one sudo prompt, one progress UI.

**What pacman's CLI can and can't do.** A single `pacman -S <names>` or
`pacman -U <files>` *does* remove packages atomically **when the removal is
declared** — the new package's `conflicts=` / `replaces=` causes pacman to pull
the conflicting/replaced installed package out **in the same transaction**. So
the common "`foo-bin` replaces `foo`", "`foo-ng replaces=foo`", and EOL-repo →
AUR transitions already work atomically today, *provided the new package goes in
via one pacman call*. What the CLI **cannot** express is an **undeclared**
remove+add: "uninstall group A and install unrelated group B as one transaction."
`pacman -R A` and `pacman -S/-U B` are two separate transactions; there is no
flag to merge them. There is also no single pacman CLI call that mixes
sync-repo adds (`-S name`) with local-file adds (`-U file`).

**libalpm can.** A single `alpm` transaction may register both additions
(`trans_add_pkg`, for syncdb packages *and* `pkg_load`'ed `.pkg.tar` files) and
removals (`trans_remove_pkg`) before one `trans_prepare` + `trans_commit`. This
is precisely the API gitaur **already drives read-only** in
`pacman::invoke::preflight_dash_u_inner` (`trans_init(NO_LOCK)` →
`pkg_load` → `trans_add_pkg` → `trans_prepare` → `trans_release`). The only
missing pieces for a real commit are: take the DB lock instead of `NO_LOCK`,
add the `trans_remove_pkg` calls, and `trans_commit` — and do it **with
privilege**. This is also the direction memory `feedback_native_libalpm_over_pacman`
already points ("`alpm` crate for DB reads+writes … own progress UI; shell out
only for the privileged final txn") — native commit retires that last shell-out.

**The privilege boundary.** Committing writes `/var/lib/pacman`, which needs
root, and gitaur deliberately runs unprivileged (it lets *pacman* escalate via
the configured escalator). The clean way to keep the one-sudo model is a small
**internal privileged subcommand**: `apply` serializes the prepared transaction
(syncdb add names + AUR file paths + remove names + flags) and re-execs
`<escalator> gaur __commit-txn <spec>`; that hidden subcommand opens alpm,
registers adds+removes, prepares, and commits — owning the install progress UI
directly (the "own progress UI" win). One escalation, one transaction, full
atomicity across repo + AUR + removals.

**Phasing this sub-feature** (so the shell ships before the native commit lands):

- *Interim (phase 3).* `apply` issues the existing pacman calls:
  `dispatch::run_repo_upgrade` / `pacman -S` for repo, `build::apply_plan`'s
  per-stratum `pacman -U` for AUR, and `pacman -R` for explicit removals.
  Declared replaces/conflicts are already atomic within each call; an *undeclared*
  remove+add is two transactions bridged by the sudo cache. Honest, shippable,
  matches today's behavior.
- *Target (phase 6).* Replace the privileged step with the native combined
  `__commit-txn`. Reuses the `preflight_dash_u_inner` machinery almost verbatim
  (drop `NO_LOCK`, add removals, commit). Gate behind a config knob
  (`native_commit = false` initially) until it's proven against the container
  suite, then flip the default.

**Resolver note.** For the cart to *know* the removals implied by a declared
replace (so `show` previews them), the resolver/preview can reuse the read-only
`trans_prepare` already in `invoke.rs`: prepare the add set, read back the
`ConflictingDeps` / replaced packages, and list them as "will remove" rows. That
makes the preview honest even in the interim phase, before native commit.

## Tab completion

Context-aware completion from the first keystroke, via a rustyline `Completer`
(`src/cli/shell/complete.rs`). Completion is **positional** — what completes
depends on the verb and the argument slot:

| Cursor position | Completes to |
| --- | --- |
| first word | command verbs (`search`, `add`, `upgrade`, `apply`, …) + `help` topics |
| arg of `search` / `add` / `info` / `upgrade` | package names — AUR pkgbases/pkgnames from the loaded index + sync-DB names |
| arg of `review` / `drop` | names currently **in the cart / last list** (the relevant small set, not all 155k) |
| arg of `help` | command verbs |
| a numeric token | no completion (numbers index the current list) |

**Name source + speed.** The index is already mmap'd and `Secondary` exposes
`by_name`, but that's a hashmap (no prefix order). For interactive Tab latency
on ~155k names, build a **sorted `Vec<&str>` of candidate names once at session
start** (pkgbases + pkgnames + syncdb names), then answer each Tab with a binary
search for the prefix range, capped (e.g. 200 shown, with a "+N more" note). The
sorted vector is cheap to build from data already in memory and keeps Tab
sub-millisecond. Globs entered literally (`python-*`) are passed through, not
expanded by Tab.

Completion shares the same name-resolution table as the `Selector` resolver, so
"what Tab offers" and "what `add <name>` accepts" never drift. History
(`$XDG_STATE_HOME/gitaur/shell_history`) gives recall of past commands across
sessions; an optional rustyline `Hinter` can ghost-suggest from history later.

## Phasing

Each phase is independently shippable and leaves the flag CLI fully working.

1. **REPL skeleton.** rustyline loop, `shell-words` parse, `Command` enum, the
   session hoist (reuse `UpgradeSession`), `help`/`quit`/Ctrl-C/Ctrl-D, history
   file. Wire the no-arg interactive branch to `shell::run`; keep `upgrade_loop`
   as the fallback the branch *was* calling (feature-gate or direct swap — TBD).
   `ShellEnv` trait + pure `dispatch` core with a scripted-fake unit test, like
   `drive`/`FakeEnv`.
2. **Read-only commands + selector core.** `search` (reuse `search.rs` query →
   numbered output, remember list), `info`, and the `Selector` parse+resolve
   (numbers/ranges/names/globs) — the reusable core every later command needs.
   Tab-completion of verbs + names.
3. **Cart + apply (interim, pacman calls).** `add`/`drop`/`remove`/`clear`/`show`
   building the `Cart`; `apply` = resolve (`build::resolve_targets`) →
   `ui::change_set_table` preview (incl. "will remove" rows from a read-only
   `trans_prepare`) → `ui::confirm` → `apply_plan` + repo `pacman -S` + explicit
   `pacman -R` + fold; failed items stay staged and badged; reuse the SIGINT
   bail. `review <sel>` gating `apply` on the reviewed set. Declared
   replaces/conflicts are atomic within each pacman call; undeclared remove+add
   is two transactions here (made atomic in phase 6). The felt payload — fresh
   installs in the shell.
4. **Upgrades in the shell.** `upgrade [glob…]` stages remaining candidates
   (reuse `recompute_remaining`); `show`/`apply` already handle mixed carts;
   port the cost overlay (sizes, build-time, `built` tag — `candidate_metrics`/
   `preview_metrics`). Retire `upgrade_loop.rs`.
5. **Polish.** `refresh`, per-root dep nesting in `show` (the route-2 nicety, now
   free), fuzzy name completion + history `Hinter`, `help <topic>`, prompt/history
   config knobs.
6. **Native combined commit (atomic add+remove).** Internal `__commit-txn`
   privileged subcommand: one libalpm transaction over repo adds + AUR file adds +
   removals, owning the install progress UI. Reuses `invoke.rs`'s transaction
   machinery (drop `NO_LOCK`, add `trans_remove_pkg`, `trans_commit`). Behind
   `native_commit` config knob; flip default once the container suite covers the
   add+remove and group-swap cases. Satisfies decision 5 fully.

## Testing

Mirrors the existing two-tier philosophy (`docs/TESTING.md`) and the loop's seams:

- **Unit** — `command::parse` (verbs, quoting), `selector::resolve` (numbers,
  ranges, out-of-range errors, glob match/no-match), and the `dispatch` core via
  a scripted `ShellEnv` fake (cart mutation, fold-on-failure, exit conditions) —
  exactly the `drive`/`FakeEnv` pattern already in `upgrade_loop.rs`.
- **Container e2e** — drive the real REPL under a PTY using the existing
  `pty-harness` dev-crate (precedent: `loop_built_tag_e2e` /
  `tests/container/extended/06_loop_built_tag.sh`). Script: `search` → `add` by
  number + glob → `show` → `apply`, asserting installed state; plus a
  Ctrl-C-bails-to-prompt case and an `upgrade`→`apply` case.

## Out of scope (this iteration)

- A fullscreen TUI (ratatui) — the decision is line-oriented REPL, not full-screen.
- Looping/changing the explicit `-Syu` flag path — it keeps its current
  one-shot `dialoguer` picker.
- Scriptable shell input files / a `-c "command"` one-liner mode (possible later;
  the dispatch core is already pure enough to support it).
- Cross-user metric sharing (already out of scope in UPDATE_LOOP).

## Code anchors

| File | Anchor | Role in the refactor |
| --- | --- | --- |
| `src/cli/dispatch.rs` | no-arg interactive branch (~`:30`) | swap `upgrade_loop::run` → `shell::run` |
| `src/cli/upgrade_loop.rs` | `UpgradeSession`, `recompute_remaining`, `drive`/`LoopEnv`, `candidate_metrics`, `preview*`, `resolve_aur`, `system_pac`/`synced_pac`, `SessionState::fold` | reuse; `drive` becomes the shell dispatch core; retire the file in phase 4 |
| `src/cli/search.rs` | `cmd_search_install`, `Row`, `label_*` | split: keep the query/rows, replace `pick` (MultiSelect) with numbered output |
| `src/ui/prompts.rs` | `confirm`, `select_pkgnames` | `confirm` reused at `apply`; `select_pkgnames` still used by the build path |
| `src/ui/change_set.rs` | `change_set_table` | the `show`/`apply` preview |
| `src/ui/tables.rs` | `select_upgrades` | **flag path only** after this — shell never calls it |
| `src/build.rs` | `resolve_targets`, `apply_plan`, `cmd_install`, `Target::{with_hint,bare}` | the apply engine |
| `src/build/review.rs` | `review()` (`proceed`/`view`/`edit`/`skip`/`abort`) | the `review` command + gated `apply` review |
| `src/pacman/invoke.rs` | `preflight_dash_u_inner` (`trans_init`/`pkg_load`/`trans_add_pkg`/`trans_prepare`), `exec_pacman`, `confirm_escalation` | native-transaction template for `__commit-txn` (add removals + `trans_commit`); "will remove" preview |
| `src/index/secondary.rs` | `by_name` | source for the sorted completion/selector name set |
| `src/error.rs` | `Interrupted` (`:68`), `UserAbort` (`:62`) | prompt-bail + decline semantics |
| `src/paths.rs` | state-dir helpers | shell history file path |
| `Cargo.toml` | UI deps block (`:104`) | add `rustyline`, `shell-words`, `globset` |
```
