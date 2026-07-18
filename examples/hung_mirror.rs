//! A git-over-HTTP mirror that answers the response headers, then hangs — the
//! smallest server that puts a fetch into a stalled *receiving* state.
//!
//! Used by the `ctrlc-refresh` screencast demo and its extended test: a real
//! mirror can hang mid-fetch, and this reproduces exactly that. Unlike
//! [`tarpit`](./tarpit.rs) (which never replies, exercising curl's idle
//! *timeout*), this replies with a valid `200` + git content-type so the client
//! transitions into "receiving the advertisement," then goes silent — the point
//! is not the timeout but that a Ctrl+C bails *promptly* instead of waiting it
//! out. That prompt bail on a silent socket relies on the gix
//! `http::Options::should_interrupt` patch: gix's own cooperative flag is only
//! polled between reads, so a transfer parked in a read is aborted by the curl
//! backend's transfer-meter callback instead.
//!
//! Usage: `hung_mirror <port>` (binds 127.0.0.1). Every request gets the same
//! reply-then-hang treatment; tests kill the process to release the threads.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .expect("usage: hung_mirror <port>")
        .parse()
        .expect("port must be u16");
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("hung_mirror listening on 127.0.0.1:{port}");
    for stream in listener.incoming().flatten() {
        // Standalone test server — no aurox thread-locals to propagate.
        #[allow(clippy::disallowed_methods)]
        thread::spawn(move || {
            // Drain the request line + headers so the client's write completes
            // and it moves on to awaiting the response.
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            // Answer with a valid smart-HTTP advertisement header, then send no
            // body: the client accepts the response and blocks reading the pack
            // advertisement that never arrives — a fetch hung mid-receive.
            // (`&stream` written through a temporary — `Write for &TcpStream`
            // trips a spurious `unused_mut` on a named binding.)
            (&stream)
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: application/x-git-upload-pack-advertisement\r\n\
                      Cache-Control: no-cache\r\n\
                      \r\n",
                )
                .ok();
            (&stream).flush().ok();
            // Hold the connection open and silent until the test kills us.
            thread::sleep(Duration::from_hours(24));
            drop(stream);
        });
    }
}
