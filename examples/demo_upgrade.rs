//! Demo driver: system upgrade in the shell with a mixed change set — a repo
//! update (`loop-repo` 1.0 → 2.0 from the local sync repo) and an AUR rebuild
//! (`test-hello` 1.0 → 1.4) in one staged transaction
//! (docs/plans/screencasts.md). `demos/seed-upgrade.sh` installs the outdated
//! 1.0 builds first, so `upgrade` has real work.
//!
//! The demo types a bare `upgrade` — whole system, like `pacman -Syu`. The
//! seed script strips the real core/extra repos from pacman.conf, so "all
//! pending updates" truthfully consists of the two seeded fixtures (see
//! `demos/seed-upgrade.sh` for why that is the only hermetic way).
//!
//! Rendered to `docs/demo/upgrade.gif` by `demos/build.sh`; run as a plain
//! test by `tests/container/extended/36_demo_upgrade.sh`.

use pty_harness::{Pty, dwell, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));
    dwell(1500);

    pty.send_human("upgrade");
    pty.expect("repo row staged", |s| has(s, "loop-repo 1.0-1 → 2.0-1"));
    pty.expect("aur row staged", |s| has(s, "test-hello 1.0-1 → 1.4-1"));
    dwell(3000);

    // The AUR rebuild needs review like any AUR change; approve it.
    pty.expect("review gate", |s| s.contains("needs review"));
    pty.send_human("approve test-hello");
    pty.expect("approved", |s| s.contains("approved test-hello"));
    dwell(1500);

    pty.send_human("apply");
    // Two elevation gates: the repo lane's partial `pacman -Syu` first…
    pty.expect("repo sudo gate", |s| s.contains("Continue?"));
    dwell(1500);
    pty.send(b"\r");

    // …then, after the streaming AUR build, a second gate for the built
    // package's `pacman -U`. The first `Continue?` stays on screen, so the
    // disclosed `-U` command line is the distinguishing needle.
    pty.expect("aur install gate", |s| has(s, "pacman -U"));
    dwell(1200);
    pty.send(b"\r");

    pty.expect("apply finished", |s| s.contains("done"));
    dwell(2500);

    pty.send_human("quit");
    pty.finish_clean();
    println!("DEMO_UPGRADE_OK");
}
