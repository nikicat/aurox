//! End-to-end driver for the no-arg `gaur` upgrade loop, used by the podman
//! test `tests/container/extended/04_loop_repo_upgrade.sh`.
//!
//! The loop only runs interactively (stdin must be a TTY), so we spawn the real
//! `gaur` binary under a PTY, parse its VT100 output into a screen grid, and
//! walk the expected UI sequence — picker → change-set preview → sudo gate →
//! "all selected upgrades applied" — pressing Enter at each prompt. Each step
//! both *drives* the loop and *asserts the UI rendered*: if a stage's text
//! never appears, we dump the screen and exit non-zero.
//!
//! `$GITAUR` (or argv[1]) points at the binary; the container test sets up an
//! installed-but-outdated repo package first so the loop has something to show.

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vt100::Parser;

const ROWS: u16 = 40;
const COLS: u16 = 100;

fn main() {
    let gaur = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("GITAUR").ok())
        .unwrap_or_else(|| "/work/target/debug/gaur".to_owned());

    let pty = NativePtySystem::default()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // No args = the upgrade loop. Inherit the container env (HOME/PATH/XDG/…)
    // so gaur finds its config, the mock mirror, pacman, sudo, and makepkg.
    let mut cmd = CommandBuilder::new(&gaur);
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    // Tracing logs share this PTY with the UI (the console layer writes to
    // stderr). The loop's UI is `ui::*`/eprintln and prints regardless, so cap
    // tracing at `warn` to keep info-level chatter from scrolling the rows we
    // assert on off the screen — the test stays deterministic in log volume.
    cmd.env("RUST_LOG", "warn");

    let mut child = pty.slave.spawn_command(cmd).expect("spawn gaur");
    drop(pty.slave);

    let reader = pty.master.try_clone_reader().expect("clone reader");
    let mut writer = pty.master.take_writer().expect("take writer");
    let rx = spawn_reader(reader);
    let mut parser = Parser::new(ROWS, COLS, 0);

    // 1. Picker renders with the outdated package as a candidate.
    expect(&mut parser, &rx, "picker", |s| {
        s.contains("Select upgrades") && s.contains("loop-repo")
    });
    send(&mut writer, b"\r"); // confirm the (default-checked repo) selection

    // 2. Change-set preview, then its confirm gate.
    expect(&mut parser, &rx, "change-set preview + confirm", |s| {
        s.contains("this batch") && s.contains("Proceed with this batch")
    });
    send(&mut writer, b"\r"); // accept (default Y)

    // 3. Sudo escalation gate for `pacman -Syu`.
    expect(&mut parser, &rx, "sudo gate", |s| s.contains("Continue?"));
    send(&mut writer, b"\r"); // accept (default Y)

    // 4. The upgrade applied and the loop's next pass found nothing left.
    expect(&mut parser, &rx, "loop completion", |s| {
        s.contains("all selected upgrades applied")
    });

    // Drain so the child exits cleanly, then confirm a clean exit.
    drop(writer);
    pump_for(&mut parser, &rx, Duration::from_secs(5));
    let status = child.wait().expect("wait gaur");
    assert!(
        status.success(),
        "gaur exited non-zero ({status:?})\n--- screen ---\n{}",
        parser.screen().contents()
    );

    println!("LOOP_E2E_OK");
}

/// Pump the PTY until `pred` holds, or panic with the screen on timeout.
fn expect<F>(parser: &mut Parser, rx: &mpsc::Receiver<Vec<u8>>, what: &str, mut pred: F)
where
    F: FnMut(&str) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if pred(&parser.screen().contents()) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {what}\n--- screen ---\n{}\n--- end ---",
            parser.screen().contents()
        );
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(bytes) => parser.process(&bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "gaur exited before {what} appeared\n--- screen ---\n{}\n--- end ---",
                parser.screen().contents()
            ),
        }
    }
}

fn send(writer: &mut Box<dyn Write + Send>, bytes: &[u8]) {
    writer.write_all(bytes).expect("write to pty");
    writer.flush().ok();
}

fn pump_for(parser: &mut Parser, rx: &mpsc::Receiver<Vec<u8>>, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match rx.recv_timeout(remaining) {
            Ok(bytes) => parser.process(&bytes),
            Err(_) => return,
        }
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    rx
}
