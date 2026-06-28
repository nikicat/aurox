//! End-to-end driver for the no-arg `gaur` upgrade loop, used by the podman
//! tests `tests/container/extended/04_loop_repo_upgrade.sh` and
//! `05_loop_size_from_synced_db.sh`.
//!
//! Spawns the real `gaur` under a PTY (via the shared [`pty_harness::Pty`])
//! and walks the expected UI sequence — picker → change-set preview → sudo gate
//! → "all selected upgrades applied" — pressing Enter at each prompt. Each step
//! both *drives* the loop and *asserts the UI rendered*: if a stage's text
//! never appears, the harness dumps the screen and panics.
//!
//! No-arg `gaur` opens the interactive shell, so the loop is reached by typing
//! `upgrade` at the prompt — which bridges to `upgrade_loop` (refresh + picker)
//! until phase 4 folds the procedure into the shell's cart.
//!
//! `$GITAUR` (or argv[1]) points at the binary; the container test sets up an
//! installed-but-outdated repo package first so the loop has something to show.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_gaur();

    // 0. The shell prompt; `upgrade` runs the (refresh +) loop.
    pty.expect("shell banner", |s| s.contains("gitaur shell"));
    pty.send(b"upgrade\r");

    // 1. Picker renders with the outdated package as a candidate.
    pty.expect("picker", |s| {
        s.contains("Select upgrades") && s.contains("loop-repo")
    });
    pty.send(b"\r"); // confirm the (default-checked repo) selection

    // 2. Change-set preview, then its confirm gate. The batch total must be a
    //    real nonzero figure: a `total  0 B` is the smoking gun of the stale-
    //    syncdb size bug (`extended/05_loop_size_from_synced_db.sh`), where the
    //    buggy code reads `download_size()` from the system syncdb whose
    //    installed-version archive sits in the pacman cache → 0. The anchored
    //    `total  0 B` substring distinguishes that from any real nonzero total
    //    (`total  812 B`, `total  1.50 KiB`, …) without the
    //    `"500 B".contains("0 B")` footgun a bare `0 B` check would hit.
    pty.expect("change-set preview + confirm", |s| {
        s.contains("this batch") && s.contains("Proceed with this batch")
    });
    let screen = pty.screen();
    assert!(
        !screen.contains("total  0 B"),
        "change-set total is `0 B` — preview sizes look stale (read from the \
         system syncdb whose installed-version archive is cached) rather than \
         the freshly synced db's new version\n--- screen ---\n{screen}\n--- end ---"
    );
    pty.send(b"\r"); // accept (default Y)

    // 3. Sudo escalation gate for `pacman -Syu`.
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r"); // accept (default Y)

    // 4. The upgrade applied and the loop's next pass found nothing left; the
    //    loop then returns to the shell prompt (not straight to exit).
    pty.expect("loop completion", |s| {
        s.contains("all selected upgrades applied")
    });

    // Back at the shell prompt — leave cleanly.
    pty.send(b"quit\r");
    pty.finish_clean();
    println!("LOOP_E2E_OK");
}
