//! End-to-end driver for the shell's *staged* repo upgrade, used by
//! `tests/container/extended/42_shell_upgrade_staged_syncdb.sh`.
//!
//! The staged lane is the repo half of `apply` when the rootless synced store
//! is populated: the frozen `sync/*.db` are staged into the system `DBPath`
//! (an elevated `install`) and pacman runs a plain `-Su` — both under one
//! consent prompt. This driver pins that gate's shape: ONE header, one line
//! per command, and — the incident regression — **no `--dbpath` anywhere on
//! screen**: pointing the privileged pacman at the private store is what
//! split the localdb on 2026-07-25 (see `pacman::sync`'s module docs).
//!
//! The `-Syu` fallback lane (store never populated) stays pinned by
//! `shell_upgrade_e2e` / extended/04.

use pty_harness::Pty;

/// Whitespace-insensitive containment (same rationale as `shell_upgrade_e2e`):
/// staged rows pad to the widest column and long lines wrap on the 100-col
/// vt100 grid, so literal matches break on padding/wrap position. Also used
/// for the *negative* `--dbpath` probe — compaction re-joins a needle the
/// grid may have wrapped mid-token, so absence on the compacted screen is
/// absence anywhere.
fn has(screen: &str, needle: &str) -> bool {
    let compact = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    compact(screen).contains(&compact(needle))
}

fn main() {
    // Never-synced state: the first-launch question comes before the banner;
    // Enter takes the default — Later — so the staged lane must work without
    // the AUR ever being set up.
    let mut pty = Pty::spawn_aurox();
    pty.expect("three-way question", |s| s.contains("sync the AUR now?"));
    pty.send(b"\r");
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // `upgrade loop-repo` refreshes (check_repo_updates=true in this test's
    // config, so the rootless store picks up the 3.0 only the local repo
    // carries) and stages the repo row. Naming the target keeps the image's
    // real core/extra upgrades out of the transaction, as in extended/04.
    pty.send(b"upgrade loop-repo\r");
    pty.expect("repo-only degradation note", |s| {
        has(s, "upgrades are repo-only")
    });
    pty.expect("staged from the rootless store", |s| {
        has(s, "loop-repo 2.0-1 → 3.0-1")
    });
    // Barrier before `apply`: the change-set total renders after the rows, so
    // waiting on it proves the table finished streaming (a send raced into a
    // mid-render screen gets dropped when rustyline re-enters raw mode).
    pty.expect("change-set total", |s| has(s, "-> total"));

    // `apply` hits the one-consent sudo gate for the staged pair. Pin the
    // shape: one elevation header, the `install` staging line, the plain
    // `pacman -Su` line — and never `--dbpath`.
    pty.send(b"apply\r");
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    let screen = pty.screen();
    assert!(
        has(&screen, "about to elevate via sudo"),
        "elevation header missing\n--- screen ---\n{screen}\n--- end ---"
    );
    assert!(
        has(&screen, "install -pDm644 -t /var/lib/pacman/sync"),
        "staging install line missing from the consent preview\n--- screen ---\n{screen}\n--- end ---"
    );
    assert!(
        has(&screen, "pacman -Su --noconfirm"),
        "plain -Su line missing from the consent preview\n--- screen ---\n{screen}\n--- end ---"
    );
    assert!(
        !has(&screen, "--dbpath"),
        "the privileged pacman was handed --dbpath — the exact corruption \
         vector the staged lane exists to prevent\n--- screen ---\n{screen}\n--- end ---"
    );
    pty.send(b"\r");

    // A clean apply prints `done` and clears the cart; `show` confirms from
    // the shell side (the script asserts the pacman-side effects).
    pty.expect("apply finished", |s| s.contains("done"));
    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_UPGRADE_STAGED_E2E_OK");
}
