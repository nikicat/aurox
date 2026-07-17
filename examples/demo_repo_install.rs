//! Demo driver: installing a plain repo package — `aurox -S repo-hello` at a
//! real shell prompt (docs/plans/screencasts.md). No AUR involved: aurox
//! recognizes the repo target and hands the transaction to pacman after the
//! explicit `Continue?` elevation gate — the pacman-parity fast path. The
//! repo-hello fixture carries a real-sized payload and a paced .install
//! scriptlet so the pacman transaction doesn't flash by in one frame.
//!
//! Rendered to `docs/demo/repo-install.gif` by `demos/build.sh`; run as a
//! plain test by `tests/container/extended/35_demo_repo_install.sh`.

use pty_harness::{Pty, back_at_prompt, dwell};

fn main() {
    let mut pty = Pty::spawn_demo_shell();
    pty.expect("demo shell prompt", |s| s.contains('\u{276F}'));
    dwell(1000);

    pty.send_human("aurox -S repo-hello");
    // A pure-repo target goes straight to the elevation gate — no review, no
    // build; the disclosed command line is the whole plan.
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    dwell(1800);
    pty.send(b"\r");

    pty.expect("back at the prompt", back_at_prompt);
    dwell(2200);

    pty.send_human("exit");
    pty.finish_clean();
    println!("DEMO_REPO_INSTALL_OK");
}
