//! End-to-end driver for the upgrade loop's already-built column, used by the
//! podman test `tests/container/extended/06_loop_built_tag.sh`.
//!
//! The test stages an installed-but-outdated AUR package whose *new*-version
//! artifact is already sitting in the build worktree (the state a build that
//! completed in an earlier batch but wasn't yet installed leaves behind). The
//! loop's read-only built-check (`build::artifacts_built`) should then flag
//! that candidate with the `built` tag in the picker. That single render is the
//! whole assertion — once it's on screen we kill `gaur` (no need to drive an
//! actual upgrade through to apply).
//!
//! No-arg `gaur` opens the interactive shell, so the loop (and its picker) is
//! reached by typing `upgrade` at the prompt — which bridges to `upgrade_loop`
//! (refresh + picker) until phase 4 folds the procedure into the shell's cart.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_gaur();

    // No-arg `gaur` is the shell; `upgrade` runs the refresh + picker loop.
    pty.expect("shell banner", |s| s.contains("gitaur shell"));
    pty.send(b"upgrade\r");

    // The picker lists the staged candidate, and because its artifact is
    // already in the worktree at the index version, the row carries the
    // `built` tag.
    pty.expect("picker with built tag", |s| {
        s.contains("test-trivial") && s.contains("built")
    });

    pty.kill();
    println!("BUILT_TAG_E2E_OK");
}
