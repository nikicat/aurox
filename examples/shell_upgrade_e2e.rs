//! End-to-end driver for the shell's `upgrade` procedure (REPL phase 4), used
//! by `tests/container/extended/04_shell_upgrade_repo.sh`.
//!
//! No-arg `gaur` opens the shell. This drives the upgrade flow against an
//! installed-but-outdated **repo** package: `upgrade` refreshes + seeds the
//! pending upgrade (auto-approved, since it's a repo row), then `apply` renders
//! the cost-overlay change-set preview, takes the transaction confirm + the
//! sudo gate, and runs the partial `pacman -Syu`. A clean apply empties the
//! cart, which `show` confirms.
//!
//! It also folds in the synced-db **size guard** (the retired
//! `05_loop_size_from_synced_db` test): the preview total must be a real nonzero
//! figure, never `total  0 B` — the smoking gun of reading sizes from the stale
//! system syncdb (whose installed-version archive is cached → `0`) instead of
//! the freshly-synced db carrying the new version.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_gaur();
    pty.expect("shell banner", |s| s.contains("gitaur shell"));

    // Refresh + seed the pending upgrades; the repo row auto-approves and shows
    // its old → new transition.
    pty.send(b"upgrade\r");
    pty.expect("repo upgrade staged", |s| {
        s.contains("loop-repo") && s.contains("1.0-1 → 2.0-1")
    });

    // Apply: the change-set preview + the transaction confirm.
    pty.send(b"apply\r");
    pty.expect("change-set preview + confirm", |s| {
        s.contains("this batch") && s.contains("Proceed with this transaction")
    });
    let screen = pty.screen();
    assert!(
        !screen.contains("total  0 B"),
        "change-set total is `0 B` — preview sizes look stale (read from the \
         system syncdb whose installed-version archive is cached) rather than \
         the freshly synced db's new version\n--- screen ---\n{screen}\n--- end ---"
    );
    pty.send(b"\r");

    // The sudo gate for the partial `pacman -Syu`.
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");

    // A clean apply clears the cart — the shell-side proof the upgrade landed.
    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_UPGRADE_E2E_OK");
}
