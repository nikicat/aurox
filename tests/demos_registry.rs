//! Tripwire keeping `demos/demos.json` the *single* demo registry.
//!
//! That one file is what `demos/build.sh` records from, what the Screencasts
//! check-run gallery renders, and what the workflow publishes per-dir as
//! `manifest.json` for the media-repo player dropdowns (compare/diff). The
//! failure mode this guards against already happened once: `ctrlc-refresh` got
//! an `examples/demo_ctrlc_refresh.rs` driver but no dropdown entry, because the
//! list lived in several hand-edited places.
//!
//! So: every `examples/demo_<name>.rs` driver must have a registry row and
//! vice-versa. Add a driver without registering it (or the reverse) and this
//! test fails pointing at `demos/demos.json` — nobody has to *know* to update
//! it, `cargo test` says so.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Demo names in the registry — `demos/demos.json` is `[[name, title], ...]`.
fn registry_names() -> BTreeSet<String> {
    let raw = std::fs::read_to_string(repo("demos/demos.json")).expect("read demos/demos.json");
    let rows: Vec<(String, String)> =
        serde_json::from_str(&raw).expect("demos/demos.json must be [[name, title], ...]");
    rows.into_iter().map(|(name, _title)| name).collect()
}

/// Demo names implied by the drivers — `examples/demo_<name>.rs`, with the
/// filename's `_` mapped back to the registry's `-` (build.sh's `name//-/_`).
fn driver_names() -> BTreeSet<String> {
    std::fs::read_dir(repo("examples"))
        .expect("read examples/")
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| {
            let stem = f.strip_prefix("demo_")?.strip_suffix(".rs")?;
            Some(stem.replace('_', "-"))
        })
        .collect()
}

#[test]
fn every_demo_driver_is_registered_and_vice_versa() {
    let registry = registry_names();
    let drivers = driver_names();

    let unregistered: Vec<_> = drivers.difference(&registry).collect();
    assert!(
        unregistered.is_empty(),
        "these examples/demo_<name>.rs drivers have no row in demos/demos.json — \
         add [\"<name>\", \"<title>\"] there (the single demo registry) and every \
         consumer, including the media-repo player dropdowns, picks it up: {unregistered:?}",
    );

    let driverless: Vec<_> = registry.difference(&drivers).collect();
    assert!(
        driverless.is_empty(),
        "these demos/demos.json rows have no examples/demo_<name>.rs driver — \
         drop the row or add the driver: {driverless:?}",
    );
}
