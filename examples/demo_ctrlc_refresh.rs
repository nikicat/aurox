//! Demo driver: Ctrl-C during a shell `refresh` bails back to the prompt
//! instead of taking the whole shell down (docs/TODO.md "Shell").
//!
//! The harness bootstraps the mirror from the fast `file://` mock AUR first;
//! this driver then repoints both aurox's config and the bootstrapped mirror
//! repo's remote at [`hung_mirror`](./hung_mirror.rs) — a server that answers
//! the HTTP headers then goes silent — so the next `refresh` stalls mid-fetch
//! the way a real hung mirror would. A `^C` then aborts *that fetch* (via the
//! gix `should_interrupt` transport patch, since gix is parked in a read) and
//! lands back at a live prompt; `quit` exits 0, which is the proof the shell
//! survived — a shell killed by the SIGINT would exit non-zero.
//!
//! Rendered to `docs/demo/ctrlc-refresh.gif` by `demos/build.sh`; run as a
//! plain test by `tests/container/extended/37_demo_ctrlc_refresh.sh`.

use pty_harness::{Pty, dwell};
use std::path::PathBuf;
use std::process::{Child, Command};

const PORT: u16 = 18790;

fn main() {
    let url = format!("http://127.0.0.1:{PORT}/aur.git");

    // The mirror the coming `refresh` will hang on. Started before the shell so
    // it is listening by the time the fetch dials in.
    let mut server = spawn_hung_mirror();
    dwell(300);

    // Point the config *and* the bootstrapped repo's stored remote at the hung
    // server: an incremental fetch dials the repo's own remote (set at bootstrap
    // time), not `mirror_url`, so both are repointed. The long idle timeout
    // keeps curl from ending the stall itself — the Ctrl-C is what bails it.
    write_config(&url);
    set_repo_remote(&url);

    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));
    dwell(1500);

    pty.send_human("refresh");
    pty.expect("fetch started", |s| s.contains("refreshing AUR mirror"));
    // The mirror answered, then went silent: the fetch is now hung. Let the
    // viewer sit with the stalled progress before the interrupt.
    dwell(3000);

    // The user's Ctrl-C, as the terminal delivers it.
    pty.send(&[0x03]);
    pty.expect("refresh interrupted", |s| {
        s.contains("refresh: interrupted")
    });
    // Back at a live prompt — hold it on screen so the recording shows the
    // shell survived rather than dying with the fetch.
    dwell(2500);

    pty.send_human("quit");
    pty.finish_clean();

    server.kill().ok();
    server.wait().ok();
    println!("DEMO_CTRLC_REFRESH_OK");
}

/// Launch `hung_mirror`, which lives next to this driver in the examples dir.
fn spawn_hung_mirror() -> Child {
    let bin = current_exe_dir().join("hung_mirror");
    Command::new(&bin)
        .arg(PORT.to_string())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()))
}

/// Overwrite aurox's config so the fetch dials the hung server and curl's idle
/// timeout stays well out of the way (the Ctrl-C is the intended end).
fn write_config(url: &str) {
    let path = config_dir().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "mirror_url = \"{url}\"\n\
             check_repo_updates = false\n\
             mirror_idle_timeout_secs = 300\n"
        ),
    )
    .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Repoint the bootstrapped bare mirror's `origin` at the hung server, since
/// the incremental fetch reads the URL from there, not from `mirror_url`.
fn set_repo_remote(url: &str) {
    let repo = state_dir().join("aur");
    let status = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["remote", "set-url", "origin"])
        .arg(url)
        .status()
        .unwrap_or_else(|e| panic!("git remote set-url in {}: {e}", repo.display()));
    assert!(
        status.success(),
        "git remote set-url failed in {}",
        repo.display()
    );
}

fn current_exe_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("exe has a parent dir")
        .to_path_buf()
}

/// Mirror of aurox's `paths::config_dir()` — `$XDG_CONFIG_HOME/aurox` (or
/// `~/.config/aurox`), so the driver rewrites the same file the shell reads.
fn config_dir() -> PathBuf {
    xdg_base("XDG_CONFIG_HOME", ".config").join("aurox")
}

/// Mirror of aurox's `paths::state_dir()` — `$XDG_STATE_HOME/aurox` (or
/// `~/.local/state/aurox`), where the bare mirror repo lives.
fn state_dir() -> PathBuf {
    xdg_base("XDG_STATE_HOME", ".local/state").join("aurox")
}

fn xdg_base(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(
        || PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(fallback),
        PathBuf::from,
    )
}
