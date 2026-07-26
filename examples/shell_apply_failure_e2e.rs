//! End-to-end driver for the shell's apply-failure resume, used by
//! `tests/container/extended/30_shell_apply_failure_keeps_cart.sh`.
//!
//! Two independent AUR packages built in the same batch: test-trivial builds
//! fine, test-fail-build's `build()` returns 1. `apply` must contain the
//! failure (smoke/28's contract, surfaced in the shell): the survivor still
//! installs behind the sudo gate, then the cart drops the row that installed
//! and keeps ONLY the failed one staged, with the shell back at a live
//! prompt — `drop` the failed row and the cart is empty, no restart needed.
//! The `.sh` asserts the end state in localdb.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    pty.send_command("add test-fail-build");
    pty.expect("staged test-fail-build", |s| {
        s.contains("staged test-fail-build")
    });
    pty.send_command("add test-trivial");
    pty.expect("staged test-trivial", |s| s.contains("staged test-trivial"));

    pty.send_command("approve *");
    pty.expect("both approved", |s| {
        s.contains("approved test-fail-build") && s.contains("approved test-trivial")
    });

    pty.send_command("apply");
    // The stratum builds both: the failure is reported, then the survivor's
    // batched install fires the sudo gate.
    pty.expect("build failure reported", |s| {
        s.contains("test-fail-build: build failed")
    });
    pty.expect("sudo gate for the survivor", |s| s.contains("Continue?"));
    pty.send(b"\r");
    pty.expect("partial-failure summary", |s| {
        s.contains("apply partly failed") && s.contains("1 installed (dropped)")
    });

    // The offender is still staged; dropping it empties the cart (the drop
    // reprints the transaction, which is now empty).
    pty.send_command("drop test-fail-build");
    pty.expect("offender dropped", |s| {
        s.contains("dropped test-fail-build")
    });
    pty.expect("cart empty after drop", |s| s.contains("cart is empty"));

    pty.send_command("quit");
    pty.finish_clean();
    println!("SHELL_APPLY_FAILURE_E2E_OK");
}
