# Bugs

## `-Sy <target>` resolves against the *system* syncdb it just bypassed

`aurox -Sy glibc-debug` refreshes, reports success, then fails to find a
package that is in a configured repo:

```
:: refreshing AUR mirror
-> 913 ref(s) updated
-> official package databases refreshed
error: unknown target(s): glibc-debug
```

`yay -Sy glibc-debug` finds it: `glibc-debug` lives in `[core-debug]`, which
is enabled in `pacman.conf` but whose `core-debug.db` the system store had
never downloaded (nothing had run a privileged `-Sy` since the repo was
enabled). yay escalates and syncs the *system* dbs, so its resolve sees it.

**What aurox does.** `-Sy`'s repo half is the rootless refresh — it writes
into the private store (`~/.local/state/aurox/syncdb/sync/`), which after the
run *does* contain `core-debug.db`. Resolution then opens
[`alpm_db::open()`](../../src/pacman/alpm_db.rs) — the **system** dbpath
(`src/build.rs:223`) — where `core-debug` still doesn't exist, so
`resolver.rs:346` reports the target unknown. aurox had the answer on disk and
didn't look at it.

The system-dbpath choice is deliberate and documented at `alpm_db::open_synced`
("resolving installs against a fresher store could plan a version pacman
wouldn't yet have"), but it's inconsistent with its neighbours: the execution
side already uses the fresh store (`pacman/invoke.rs:52` `open_synced`), and
`apply` stages the frozen dbs into the system `DBPath` before pacman runs
(`SyncDbStaging`) — so a plan made against the private store *is* the plan
pacman executes. `-Sy` in particular is the user explicitly asking to sync;
answering out of a store the sync deliberately skipped is the surprise.

Worth checking whether the same staleness hits `cmd_search` (`search.rs:201`,
also `open()`) — a repo the system db lacks would be missing from search
results too.

Fixed ones live in git history and in the regression test that pins each — not
here.

Roadmaps, by concern: the shell's remaining atomic-transaction work in
[`shell-ui.md`](shell-ui.md), GPG key import in
[`gpg-key-auto-import.md`](gpg-key-auto-import.md), screencasts in
[`screencasts.md`](screencasts.md), the `-Syu` discoverability / `-Qf` / `-G`
ideas in [`../COMPARISON.md`](../COMPARISON.md)'s "Open design questions",
smaller UX items in [`../TODO.md`](../TODO.md), and planned container tests in
`../../tests/container/extended/.scope`.
