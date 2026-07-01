//! End-to-end driver for the bare-term launch `gaur <term>…`, used by
//! `tests/container/extended/08_shell_search_seed.sh`.
//!
//! `gaur <term>…` interactively opens the shell *seeded* with that search — the
//! same result as starting the shell and typing `search <term>…`. There is no
//! picker any more (the REPL is the one interactive surface). This spawns the
//! real `gaur test-trivial` under a PTY and asserts:
//!
//! ```text
//!   (launch)  → shell banner, then the seeded numbered result list, at a prompt
//!   add 1     → the row is addressable by its number (seeded list remembered)
//!   quit      → clean exit
//! ```
//!
//! The `.sh` runs `gaur -Sy` first so the on-disk index can classify
//! `test-trivial` as an AUR package (the shell does not fetch at startup).

use pty_harness::Pty;

fn main() {
    // Launch straight into the seeded search — the exact-name regex keeps the
    // list to the single `test-trivial` fixture so `add 1` is unambiguous.
    let mut pty = Pty::spawn_gaur_args(&["^test-trivial$"]);

    // The shell still prints its banner…
    pty.expect("shell banner", |s| s.contains("gitaur shell"));
    // …and the seeded search ran before the prompt: the numbered row is on
    // screen without the user typing `search`.
    pty.expect("seeded result row", |s| {
        s.contains("aur/test-trivial") && s.contains("  1")
    });

    // The seeded list is remembered, so the row is addressable by its number —
    // proof the launch went through the same `search` dispatch as typing it.
    pty.send(b"add 1\r");
    pty.expect("staged by number", |s| s.contains("staged test-trivial"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_SEARCH_SEED_E2E_OK");
}
