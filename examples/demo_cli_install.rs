//! Demo driver: one-shot CLI install of an AUR package — `aurox -S
//! test-hello` typed at a real shell prompt (docs/plans/screencasts.md).
//!
//! Rendered to `docs/demo/cli-install.gif` by `demos/build.sh`; run as a
//! plain test by `tests/container/extended/34_demo_cli_install.sh`. The
//! story beats: the PKGBUILD review prompt (Enter = approve default), the
//! streaming fixture build, and the explicit `Continue?` gate before the
//! privileged `pacman -U`.

use pty_harness::{Pty, back_at_prompt, dwell};

fn main() {
    let mut pty = Pty::spawn_demo_shell();
    pty.expect("demo shell prompt", |s| s.contains('\u{276F}'));
    dwell(1000);

    pty.send_human("aurox -S test-hello");
    // Resolve + plan, then the per-pkgbase PKGBUILD review gates the build;
    // Enter takes the default action, approve.
    pty.expect("review prompt", |s| s.contains("review —"));
    dwell(2200);
    pty.send(b"\r");

    // The fixture build streams its fake compile log, then the sudo gate.
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    dwell(1600);
    pty.send(b"\r");

    // pacman installs and aurox exits: bash prints a fresh `❯` prompt as the
    // last line (the typed one has long scrolled off with the build output).
    pty.expect("back at the prompt", back_at_prompt);
    dwell(2500);

    pty.send_human("exit");
    pty.finish_clean();
    println!("DEMO_CLI_INSTALL_OK");
}
