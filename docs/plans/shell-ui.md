# Plan: one atomic add+remove transaction for the shell's `apply`

The REPL shell this plan started as — cart, approval gate, selectors, tab
completion, `upgrade`/`show`/`apply` — is **shipped**. What it does and why is
documented where it can't drift from the code:

- [`../RESOLUTION_FLOW.md`](../RESOLUTION_FLOW.md) — the control flow from
  `search` to installed: resolve-at-`add`, the frozen plan, the approval gate,
  what `apply` executes and in which order.
- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — where the shell sits in the
  module map, and the consent/provider machinery it drives.
- The module docs under `src/cli/shell/` — `complete.rs` carries the positional
  completion table, `cart.rs` the cart/resolution contract, `command.rs` the
  `Verb` single-source-of-truth rule.

One decision from the original review is **not** implemented, and it is the
only reason this doc still exists.

## The open decision: `apply` should be one transaction

The motivating case: one package (or package *group*) replaces another, with no
window where the old set is gone but the new set isn't in yet — and one sudo
prompt, one progress UI.

**What pacman's CLI can and can't do.** A single `pacman -S <names>` or
`pacman -U <files>` *does* remove packages atomically **when the removal is
declared** — the new package's `conflicts=` / `replaces=` makes pacman pull the
conflicting/replaced installed package out **in the same transaction**. So the
common "`foo-bin` replaces `foo`", "`foo-ng replaces=foo`", and EOL-repo → AUR
transitions already work atomically, *provided the new package goes in via one
pacman call*. What the CLI **cannot** express is an **undeclared** remove+add
("uninstall group A and install unrelated group B as one transaction"): `pacman
-R A` and `pacman -S/-U B` are two transactions, and no single CLI call mixes
sync-repo adds (`-S name`) with local-file adds (`-U file`).

**libalpm can.** A single `alpm` transaction may register both additions
(`trans_add_pkg`, for syncdb packages *and* `pkg_load`'ed `.pkg.tar` files) and
removals (`trans_remove_pkg`) before one `trans_prepare` + `trans_commit`. This
is precisely the API aurox **already drives read-only** in
`pacman::preflight` (`trans_init(NO_LOCK)` → `pkg_load`/`sync_sysupgrade` →
`trans_add_pkg` → `trans_prepare` → `trans_release` — the `-U` and `-Su`
simulations behind the sysupgrade gate). The only missing pieces for a real
commit: take the DB lock instead of `NO_LOCK`, add the `trans_remove_pkg`
calls, `trans_commit` — and do it **with privilege**.

**The privilege boundary.** Committing writes `/var/lib/pacman` (root), and
aurox runs unprivileged (it lets *pacman* escalate). The clean way to keep
one-sudo: a small **internal privileged subcommand** — `apply` serializes the
prepared transaction (syncdb add names + AUR file paths + remove names + flags)
and re-execs `<escalator> aurox __commit-txn <spec>`, which opens alpm,
registers adds+removes, prepares, commits, and owns the install progress UI.
One escalation, one transaction, full atomicity across repo + AUR + removals.

## Where it stands

*Today (shipped).* `apply` issues pacman calls: the repo lane via
`pacman -Su` against the frozen sync db, AUR via per-stratum `pacman -U`,
removals via `pacman -R`. Declared replaces/conflicts are atomic **within** each
call; an undeclared remove+add is two transactions bridged by the sudo cache.
Honest and shippable — it matches what every other helper does.

*Target.* Replace the privileged step with the native combined `__commit-txn`,
behind a `native_commit` config knob; flip the default once the container suite
covers the add+remove and group-swap cases. Reuses `invoke.rs`'s transaction
machinery, and follows the "native libalpm over shelling to pacman" preference
(reads and writes through the `alpm` crate; shell out only for the privileged
step).

Riding along with it: "will remove" rows read back from the read-only
`trans_prepare` — prepare the add set, read back `ConflictingDeps` / replaced
packages, and list them in the cart preview, so a declared replace is visible
before `apply` rather than only in pacman's output.

## Also still open (small)

- Optional config knobs for the prompt string and history size. Nobody has
  asked; the defaults have not been a complaint.
