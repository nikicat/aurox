//! End-to-end driver for the add-time declared-conflict reject, used by
//! `tests/container/extended/41_shell_conflict_reject.sh`.
//!
//! `test-xconflict-bin` declares `conflicts=('test-xconflict')` (no
//! `replaces=`). The shell resolves the *whole cart* at `add`, so staging both
//! must fail the conflict check up front and roll the cart back — the "a cart
//! with conflicting items is impossible" guarantee — rather than pacman's
//! prepare failing at apply, after the build:
//!
//! ```text
//!   add test-xconflict       → staged (aur)            ← resolves fine alone
//!   add test-xconflict-bin   → add rejected — … conflicts with … ; cart unchanged
//!   show                     → row `1 aur review test-xconflict` — the base
//!                              is the only staged install
//! ```
//!
//! The `.sh` runs `aurox -Sy` first so the index carries both AUR entries, and
//! after this driver exits clean asserts neither package is installed (the
//! reject applied nothing).

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // The base AUR package resolves + freezes fine on its own.
    pty.send_command("add test-xconflict");
    pty.expect("base staged from the AUR", |s| {
        has(s, "staged test-xconflict (aur)")
    });

    // The `-bin` declares `conflicts=test-xconflict`, which is co-staged. The
    // whole-cart resolve at `add` runs the conflict check and rejects — the
    // cart rolls back, so nothing new stages.
    pty.send_command("add test-xconflict-bin");
    pty.expect("conflict rejected at add", |s| {
        has(s, "add rejected") && has(s, "conflicts with")
    });

    // The reject preserved the existing cart: row 1 of the table is the base,
    // still the only staged install. The needle must be the *numbered* row —
    // `show` is the only verb that prints numbers, while the txn header
    // ("… 1 to install") is already on screen from the first `add`'s summary,
    // so matching it acks nothing and the `quit` below races rustyline
    // re-arming (the 6h CI hang of run 29876293421). Don't test for the
    // -bin's absence either — the reject line still names it.
    pty.send_command("show");
    pty.expect("only the base survived", |s| {
        has(s, "1 aur review test-xconflict")
    });

    pty.send_command("quit");
    pty.finish_clean();
    println!("SHELL_CONFLICT_E2E_OK");
}
