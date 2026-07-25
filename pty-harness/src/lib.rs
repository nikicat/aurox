//! Shared PTY harness for the aurox e2e example drivers (`examples/*_e2e.rs`
//! and the `demo_*.rs` recorders).
//!
//! The shell only runs interactively (stdin must be a TTY), so each driver
//! spawns the real `aurox` binary under a PTY, parses its VT100 output into a
//! screen grid, and walks the expected UI sequence. The mechanics —
//! spawn, read pump, [`Pty::expect`]/[`Pty::send`], clean teardown — are
//! identical across scenarios; only the sequence of expectations differs.
//!
//! This lives in its own crate, pulled in as a path **dev-dependency**, rather
//! than as a module inside one example: an example is a bin crate with no
//! external API, so a shared module there can't satisfy both `unreachable_pub`
//! (no bare `pub`) and `clippy::redundant_pub_crate` (no `pub(crate)` in a
//! private module). Here the drivers are genuine external users, so the API is
//! plainly `pub` and neither lint applies. Each scenario stays a small example
//! that `use pty_harness::Pty;` and scripts its own flow — adding one is a new
//! file, not a branch in a growing dispatch.

use cast::CastRecorder;
use crossbeam_channel::{Receiver, Sender, after, select, unbounded};
use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use vt100::Parser;

mod cast;

const ROWS: u16 = 40;
const COLS: u16 = 100;

/// The patience bound shared by every wait in the harness: [`Pty::expect`]'s
/// absolute deadline for a predicate, and [`Pty::finish`]'s *silence* bound
/// on the wait for the exit — "a healthy aurox would have said something by
/// now". One constant on purpose, and anything a healthy session does (a
/// slow instrumented step under coverage, a profraw flush at exit) fits
/// well inside it.
const PATIENCE: Duration = Duration::from_secs(45);

/// How a bounded [`Pty::try_expect`] watch resolved. A dedicated tri-state
/// rather than a bool: for a probe, "aurox exited" is a different finding
/// than "still running but silent" — collapsing them is what cost issue
/// #59's first failures their diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The predicate held over the screen.
    Matched,
    /// The deadline passed without the predicate ever holding.
    TimedOut,
    /// aurox exited before the predicate held (the screen stays readable).
    Exited,
}

/// Everything a scenario can hear from `aurox`, funneled into one channel so
/// every wait in the harness is a single blocking `select`: output bytes
/// from the reader thread, the exit status from the waiter thread parked in
/// [`Child::wait`]. Two senders means no cross-sender ordering — an `Exited`
/// can overtake the final output bytes — so consumers treat it as data to
/// hold, never as end-of-stream; the channel *disconnecting* (both threads
/// done, all messages drained) is the gone-for-good signal.
enum Msg {
    Bytes(Vec<u8>),
    Exited(ExitStatus),
}

/// A one-shot deadline channel from [`after`] — fires once, then never
/// again. Time enters the harness only as `Duration` budgets turned into
/// these; no call site reads a clock. The timer's *scope* is the wait's
/// semantics: created once outside a pump loop it bounds the whole wait
/// ([`Pty::try_expect`]), created fresh per pump it bounds silence
/// ([`Pty::finish`]).
type Timer = Receiver<Instant>;

/// What one [`Pty::pump_one`] step observed — *after* the uniform state
/// update, so output bytes are already in the screen and an exit status
/// already held in `exit_status`. Callers match on this to apply only their
/// own termination policy; the [`Msg`] decode lives at that one site.
enum Pumped {
    /// Output bytes arrived and advanced the screen.
    Bytes,
    /// The waiter reported the exit; the status is now in `exit_status`.
    Exited,
    /// The caller's [`Timer`] fired before anything arrived.
    Timeout,
    /// Both feeder threads are done and the queue is drained — nothing more
    /// will ever arrive. The gone-for-good signal.
    Disconnected,
}

/// A spawned `aurox` under a PTY, with its screen parser and I/O channels.
///
/// `_master` is held only to keep the PTY open for the process's lifetime —
/// the reader/writer are derived from it.
pub struct Pty {
    parser: Parser,
    rx: Receiver<Msg>,
    writer: Box<dyn Write + Send>,
    /// Out-of-band cancel for the waiter thread parked in `wait()` —
    /// [`ChildKiller::clone_killer`] exists for exactly this split. On unix
    /// it sends SIGHUP with no KILL escalation; enough, because aurox
    /// installs no SIGHUP handler, and a truly immune child is contained by
    /// run.sh's per-test timeout and the container teardown above us.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// An exit status a pump consumed mid-scenario: `Msg::Exited` races the
    /// final output bytes, so any [`Self::expect`] may receive it and must
    /// not lose it — [`Self::finish`] is its consumer.
    exit_status: Option<ExitStatus>,
    _master: Box<dyn MasterPty + Send>,
    /// Typing-jitter RNG for [`Self::send_human`] — fixed seed, so a demo's
    /// keystroke rhythm is the same on every run.
    rng: fastrand::Rng,
}

impl Pty {
    /// Spawn `aurox` (from argv[1], else `$AUROX`, else the default debug path)
    /// with no args — the interactive shell — inheriting the container env so
    /// it finds its config, the mock mirror, pacman, sudo, and makepkg.
    pub fn spawn_aurox() -> Self {
        Self::spawn_aurox_args(&[])
    }

    /// Like [`Self::spawn_aurox`] but passes `args` to `aurox`. Used to drive the
    /// bare-term launch (`aurox <term>…`), which opens the shell *seeded* with
    /// that `search` instead of the plain prompt.
    pub fn spawn_aurox_args(args: &[&str]) -> Self {
        let aurox = resolve_aurox();
        let mut cmd = CommandBuilder::new(&aurox);
        for a in args {
            cmd.arg(a);
        }
        let title = if args.is_empty() {
            "aurox".to_owned()
        } else {
            format!("aurox {}", args.join(" "))
        };
        Self::spawn(cmd, &[], &title)
    }

    /// An interactive bash under the PTY, for demo drivers that showcase a
    /// CLI invocation — typing `aurox -S …` at a shell prompt is then part of
    /// the recording, not off-screen argv. `--norc` keeps the session
    /// hermetic; `PS1` is a minimal colored `❯`, and the resolved aurox
    /// binary's directory is prepended to `PATH` so the typed command is a
    /// bare `aurox`.
    pub fn spawn_demo_shell() -> Self {
        let aurox = resolve_aurox();
        let bin_dir = std::path::Path::new(&aurox)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default());
        let mut cmd = CommandBuilder::new("bash");
        cmd.arg("--norc");
        cmd.arg("-i");
        let overrides = [
            // \[…\] wraps the color codes as zero-width for readline.
            ("PS1", "\\[\\e[1;36m\\]\u{276F}\\[\\e[0m\\] ".to_owned()),
            ("PATH", path),
        ];
        Self::spawn(cmd, &overrides, "demo shell")
    }

    /// Common spawn tail: inherit the container env (so aurox finds its
    /// config, the mock mirror, pacman, sudo, and makepkg), pin `TERM`, apply
    /// caller `overrides` last so inheritance can't clobber them, and wire up
    /// the PTY, reader thread, and (env-gated) cast recorder.
    fn spawn(mut cmd: CommandBuilder, overrides: &[(&str, String)], title: &str) -> Self {
        let pty = NativePtySystem::default()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("a pty pair should open on any Linux with /dev/ptmx");

        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        // The test image's Dockerfile sets `RUST_LOG=off` so the console
        // tracing layer doesn't share this PTY with the UI we assert on (a
        // stray WARN floods the screen). All assertable output comes from
        // `ui::*` eprintlns, which run regardless of the tracing filter.
        for (k, v) in overrides {
            cmd.env(k, v);
        }

        let child = pty
            .slave
            .spawn_command(cmd)
            .expect("the aurox binary should exist and be spawnable under the pty");
        drop(pty.slave);

        let reader = pty
            .master
            .try_clone_reader()
            .expect("the pty master should hand out a reader");
        let writer = pty
            .master
            .take_writer()
            .expect("the pty writer should still be untaken (taken once, here)");
        let killer = child.clone_killer();
        let (tx, rx) = unbounded();
        spawn_reader(reader, CastRecorder::from_env(title), tx.clone());
        spawn_waiter(child, tx);
        Self {
            parser: Parser::new(ROWS, COLS, 0),
            rx,
            writer,
            killer,
            exit_status: None,
            _master: pty.master,
            rng: fastrand::Rng::with_seed(0x5EED),
        }
    }

    /// The current screen contents as plain text (ANSI already interpreted).
    pub fn screen(&self) -> String {
        self.parser.screen().contents()
    }

    /// Block for one event — the funnel or the `timer` firing — and apply
    /// the uniform state update: bytes advance the screen, an exit status is
    /// held in `exit_status` (exit races the final bytes across the two
    /// senders, so it is data to hold, never end-of-stream — see [`Msg`]).
    /// The one site that decodes [`Msg`]; callers own only their termination
    /// policy, expressed as a match on [`Pumped`] and the [`Timer`]'s scope.
    fn pump_one(&mut self, timer: &Timer) -> Pumped {
        select! {
            recv(self.rx) -> msg => match msg {
                Ok(Msg::Bytes(bytes)) => {
                    self.parser.process(&bytes);
                    Pumped::Bytes
                }
                Ok(Msg::Exited(status)) => {
                    self.exit_status = Some(status);
                    Pumped::Exited
                }
                Err(_) => Pumped::Disconnected,
            },
            recv(timer) -> _ => Pumped::Timeout,
        }
    }

    /// Pump the queued post-exit tail until the channel disconnects (the
    /// reader thread ends on EOF, so this is the usual, near-immediate exit)
    /// or `quiet` passes with nothing new — the straggler case, where a
    /// grandchild inherited the slave fd and keeps the PTY open.
    fn drain(&mut self, quiet: Duration) {
        loop {
            match self.pump_one(&after(quiet)) {
                Pumped::Timeout | Pumped::Disconnected => return,
                Pumped::Bytes | Pumped::Exited => {}
            }
        }
    }

    /// Pump the PTY until `pred` holds over the screen — the panicking face
    /// of [`Self::try_expect`], dying with the screen when [`PATIENCE`] runs
    /// out or `aurox` exits first.
    pub fn expect<F>(&mut self, what: &str, pred: F)
    where
        F: FnMut(&str) -> bool,
    {
        match self.try_expect(PATIENCE, pred) {
            Expectation::Matched => {}
            Expectation::TimedOut => panic!(
                "timed out waiting for {what}\n--- screen ---\n{}\n--- end ---",
                self.parser.screen().contents()
            ),
            Expectation::Exited => panic!(
                "aurox exited before {what} appeared\n--- screen ---\n{}\n--- end ---",
                self.parser.screen().contents()
            ),
        }
    }

    /// Non-panicking [`Self::expect`] with a caller-chosen deadline: pump the
    /// PTY until `pred` holds and report how the watch resolved. For probe
    /// drivers that classify a failure and keep interrogating the session
    /// (issue #59's second-`^C` probe) instead of dying on the first miss.
    /// The screen stays readable via [`Self::screen`] on every outcome.
    pub fn try_expect<F>(&mut self, timeout: Duration, mut pred: F) -> Expectation
    where
        F: FnMut(&str) -> bool,
    {
        // One timer for the whole wait — an absolute bound, deliberately not
        // the per-pump silence bound `finish` uses: streamed redraws (an
        // indicatif spinner ticks ~10Hz) would keep resetting a per-pump
        // timer, so a never-matching predicate would pump forever.
        let timer = after(timeout);
        loop {
            if pred(&self.parser.screen().contents()) {
                return Expectation::Matched;
            }
            match self.pump_one(&timer) {
                Pumped::Timeout => return Expectation::TimedOut,
                Pumped::Disconnected => return Expectation::Exited,
                Pumped::Bytes | Pumped::Exited => {}
            }
        }
    }

    /// Write bytes to the PTY (e.g. `b"\r"` to confirm a prompt).
    pub fn send(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .expect("aurox should still be reading the pty");
        self.writer.flush().ok();
    }

    /// Block until aurox's REPL prompt is armed ([`at_prompt`]).
    ///
    /// The ack every command send needs, and the reason it's a method rather
    /// than each driver's business: a content needle ("staged foo") proves
    /// output *started*, and rustyline discards whatever arrived before it
    /// re-entered raw mode — so a command sent on a content ack can vanish,
    /// leaving the scenario waiting on a reply to something aurox never read.
    /// [`Self::send_command`] calls this for you; demo drivers that type with
    /// [`Self::send_human`] call it themselves.
    pub fn wait_for_prompt(&mut self) {
        self.expect("the aurox prompt to be armed", at_prompt);
    }

    /// Type one REPL command line, once the prompt is actually armed.
    ///
    /// The whole point is that the wait is not optional: content `expect`s
    /// around the call stay *assertions* about what the session showed,
    /// instead of doubling as timing barriers that a table printed after them
    /// silently invalidates.
    pub fn send_command(&mut self, line: &str) {
        self.wait_for_prompt();
        self.send(line.as_bytes());
        self.send(b"\r");
    }

    /// Demo pacing: type `line` character by character with a human-ish,
    /// *deterministic* rhythm, then Enter after a beat. rustyline echoes each
    /// keystroke, so in a cast recording this reads as live typing. Only call
    /// at a prompt — [`Self::wait_for_prompt`] first for the aurox REPL,
    /// [`back_at_prompt`] for the demo bash shell — since buffered input sent
    /// before the reader arms is dropped; the per-char trickle itself is what
    /// a terminal delivers anyway.
    pub fn send_human(&mut self, line: &str) {
        let mut buf = [0u8; 4];
        for c in line.chars() {
            self.send(c.encode_utf8(&mut buf).as_bytes());
            std::thread::sleep(Duration::from_millis(self.rng.u64(35..80)));
        }
        std::thread::sleep(Duration::from_millis(180));
        self.send(b"\r");
    }

    /// Close the input, drain remaining output, and assert `aurox` exited 0.
    /// Consumes the harness — the scenario is over.
    pub fn finish_clean(self) {
        let (status, screen) = self.finish();
        assert!(
            status.success(),
            "aurox exited non-zero ({status:?})\n--- screen ---\n{screen}"
        );
    }

    /// Like [`Self::finish_clean`] but asserting a specific exit code — e.g.
    /// 130 (128+SIGINT) for the idle-prompt Ctrl-C quit, which a wrapper must
    /// be able to tell apart from `quit`'s 0.
    pub fn finish_with_code(self, expected: u32) {
        let (status, screen) = self.finish();
        assert_eq!(
            status.exit_code(),
            expected,
            "aurox exit code ({status:?})\n--- screen ---\n{screen}"
        );
    }

    /// Shared teardown: pump until the waiter thread reports `aurox`'s exit,
    /// and hand back that status plus the final screen for the assertion.
    /// No input-close is involved: `_master` holds the PTY open throughout,
    /// so aurox never sees EOF — it exits because it processed the
    /// scenario's final command.
    ///
    /// The wait is bounded by [`PATIENCE`] of *silence* — a `Timeout` from a
    /// full-length recv means no output and no exit for 45s, and a driver
    /// bug that loses the final command (rustyline drops input buffered
    /// before it re-arms) leaves aurox exactly that: alive and mute at the
    /// prompt. An unbounded wait here once held the container, and the whole
    /// CI job, to the runner's 6h kill (run 29876293421); now the killer
    /// handle unparks the waiter and the panic carries the final screen — a
    /// red test with a diagnosis instead of a hang. (A pathological child
    /// streaming forever without exiting outlives this bound; run.sh's
    /// per-test timeout is the layer that catches it.)
    fn finish(mut self) -> (ExitStatus, String) {
        let status = loop {
            // Checked first: `Msg::Exited` races the final output bytes, so
            // an earlier `expect` pump may already have banked it.
            if let Some(status) = self.exit_status.take() {
                break status;
            }
            // A fresh timer per pump: [`PATIENCE`] of silence, reset by any
            // sign of life.
            match self.pump_one(&after(PATIENCE)) {
                Pumped::Timeout => {
                    self.killer.kill().ok();
                    panic!(
                        "no output and no exit for {}s after the scenario's last command — was it lost?\n--- screen ---\n{}\n--- end ---",
                        PATIENCE.as_secs(),
                        self.parser.screen().contents()
                    );
                }
                // Both threads gone yet no exit status ever arrived: the
                // waiter panicked (its `wait()` failed). Surface that
                // rather than invent a status.
                Pumped::Disconnected => panic!(
                    "pty channel closed without an exit status — waiter thread died\n--- screen ---\n{}\n--- end ---",
                    self.parser.screen().contents()
                ),
                Pumped::Bytes | Pumped::Exited => {}
            }
        };
        self.drain(Duration::from_secs(2));
        (status, self.screen())
    }

    /// Kill `aurox` and reap it — for scenarios whose assertion is complete once
    /// a screen rendered, with no clean exit path to drive.
    pub fn kill(mut self) {
        self.killer.kill().ok();
        // Bounded reap: pump until the waiter reports the exit, so the child
        // is gone when this returns. Five quiet seconds without it means a
        // HUP-immune child — left to the container/per-test-timeout layers
        // rather than wedging a teardown that exists to be unceremonious.
        while self.exit_status.is_none() {
            match self.pump_one(&after(Duration::from_secs(5))) {
                Pumped::Timeout | Pumped::Disconnected => return,
                Pumped::Bytes | Pumped::Exited => {}
            }
        }
    }
}

/// The aurox binary under test: argv[1] → `$AUROX` → the default debug path.
fn resolve_aurox() -> String {
    std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AUROX").ok())
        .unwrap_or_else(|| "/work/target/debug/aurox".to_owned())
}

/// Demo pacing: hold the current screen so a viewer can read it. Output that
/// arrives meanwhile is still pumped into the cast by the reader thread with
/// true timing; only the driver waits.
pub fn dwell(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// True when the [`Pty::spawn_demo_shell`] prompt is the last non-blank line
/// — the foreground command finished and bash is reading again. Counting `❯`
/// occurrences breaks once earlier prompt lines scroll off the vt100 grid.
pub fn back_at_prompt(screen: &str) -> bool {
    screen
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim() == "\u{276F}")
}

/// True when aurox's own REPL prompt (`aurox> `, `aurox [2 staged]> `) is the
/// last non-blank line: the command finished rendering and rustyline is
/// reading again.
///
/// The predicate behind [`Pty::send_command`] / [`Pty::wait_for_prompt`],
/// public for drivers that need it inside a compound expectation. Prefer
/// those methods: a content needle ("staged foo") says the output *started*,
/// not that it finished — a cart mutation still has a transaction table to
/// print — and input that lands before rustyline re-enters raw mode is
/// discarded, so a command sent on a content ack vanishes and the scenario
/// deadlocks. The counterpart to [`back_at_prompt`], which does the same job
/// for the demo bash shell.
pub fn at_prompt(screen: &str) -> bool {
    screen
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| {
            let line = l.trim_start();
            line.starts_with("aurox") && line.contains('>')
        })
}

/// True when `prompt` appears *after* the last occurrence of `marker` — the
/// ack for a prompt whose text repeats within one session.
///
/// The sudo gate asks `Continue?` once per elevation, and the answered first
/// one stays on the vt100 grid, so a bare `contains("Continue?")` acks the
/// *previous* gate. Reaching instead for the disclosed command line
/// (`pacman -U`) trades one stale match for another: the elevation preview
/// prints *before* dialoguer arms the prompt, so that needle fires early and
/// the answer races into a reader that isn't there yet. Only "the prompt
/// rendered since the preview" proves this gate is live:
/// `armed_after(s, "pacman -U", "Continue?")`.
pub fn armed_after(screen: &str, marker: &str, prompt: &str) -> bool {
    let (screen, marker, prompt) = (compact(screen), compact(marker), compact(prompt));
    screen
        .rfind(&marker)
        .is_some_and(|i| screen[i..].contains(&prompt))
}

/// Whitespace-insensitive containment: table columns pad to the widest staged
/// row and long lines wrap on the 100-col vt100 grid, so a literal
/// `1.0-1 → 2.0-1` match breaks whenever padding widths or the wrap position
/// shift. Compacting both sides makes the match immune to both.
pub fn has(screen: &str, needle: &str) -> bool {
    compact(screen).contains(&compact(needle))
}

/// Strip every whitespace character, so a match can't hinge on padding widths
/// or where a 100-col wrap fell.
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut recorder: Option<CastRecorder>,
    tx: Sender<Msg>,
) {
    // pty-harness is a standalone dev crate with no aurox thread-locals to
    // propagate, so the `context::spawn` rule (src/context.rs) doesn't apply.
    #[allow(clippy::disallowed_methods)]
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            // Tee into the cast here, at read time, so event timing reflects
            // when output appeared — not when `expect` got around to recv it.
            if let Some(rec) = recorder.as_mut()
                && let Err(err) = rec.record(&buf[..n])
            {
                eprintln!("pty-harness: cast recording stopped: {err}");
                recorder = None;
            }
            if tx.send(Msg::Bytes(buf[..n].to_vec())).is_err() {
                // Receiver gone (scenario killed) — stop pumping, but still
                // fall through to flush the cast's carried bytes below.
                break;
            }
        }
        if let Some(rec) = recorder.as_mut() {
            rec.finish().ok();
        }
    });
}

/// Park a thread in the blocking [`Child::wait`] — beyond the non-blocking
/// `try_wait`, the only exit-observation API `portable_pty` has (Unix offers
/// no portable timed wait for it to wrap) — and convert completion into a
/// [`Msg`], so the harness waits stay purely event-driven. The thread is
/// unparked either by the child exiting or by the killer handle on
/// `finish`'s deadline path; `wait()` also reaps, so no zombie outlives it.
fn spawn_waiter(mut child: Box<dyn Child + Send + Sync>, tx: Sender<Msg>) {
    // Same thread-locals rationale as in `spawn_reader`.
    #[allow(clippy::disallowed_methods)]
    std::thread::spawn(move || {
        let status = child
            .wait()
            .expect("aurox should be waitable exactly once (this thread is the only reaper)");
        tx.send(Msg::Exited(status)).ok();
    });
}
