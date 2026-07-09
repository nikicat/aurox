//! End-to-end driver for the shell's `info` source routing, used by
//! `tests/container/extended/10_shell_info_repo.sh`.
//!
//! `info` must describe a sync-repo package from the sync DBs and an AUR
//! package from the index — and the repo lookup runs first, so a name pacman
//! owns never shows a same-named AUR entry. The shell is interactive (stdin
//! must be a TTY), so this spawns the real no-arg `gaur` under a PTY:
//!
//! ```text
//!   info repo-base     → `Repository      : local-repo` block (sync-DB hit;
//!                        before the routing fix this printed "not in AUR")
//!   info test-trivial  → `Repository      : aur` block (index hit)
//!   quit               → clean exit
//! ```
//!
//! The `.sh` runs `gaur -Sy` first so the AUR half has an index to answer
//! from; the repo half must work regardless.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_gaur();
    pty.expect("shell banner", |s| s.contains("gitaur shell"));

    // A sync-repo package: the info block must come from the sync DBs — the
    // AUR index doesn't know this name, so the old index-only lookup answered
    // "not in AUR" here.
    pty.send(b"info repo-base\r");
    pty.expect("repo info block", |s| {
        s.contains("Repository      : local-repo") && s.contains("Name            : repo-base")
    });

    // An AUR-only package still routes to the index.
    pty.send(b"info test-trivial\r");
    pty.expect("aur info block", |s| {
        s.contains("Repository      : aur") && s.contains("Name            : test-trivial")
    });

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_INFO_E2E_OK");
}
