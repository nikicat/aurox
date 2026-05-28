//! Rootless refresh of the official-repo sync databases — gitaur's native
//! equivalent of `checkupdates(1)`, without the `fakeroot` dance.
//!
//! `pacman -Sy` needs root because it writes the downloaded DBs into
//! `DBPath/sync/` and the pacman *frontend* enforces `EUID == 0`. libalpm
//! itself enforces no such thing: `alpm_db_update` just writes wherever the
//! handle's dbpath points. So we open an [`Alpm`] handle aimed at a *private*,
//! user-writable dbpath (its `local` symlinked to the system one), register the
//! configured repos, and call [`update`] — a normal-user download into gitaur's
//! state dir. No root, no `fakeroot`, no subprocess.
//!
//! libalpm drives the download through its own libcurl backend and reports
//! per-file [`AnyDownloadEvent`]s, which [`DlProgress`] turns into one indicatif
//! byte-row per repo DB. The caller shares its [`MultiProgress`] so those rows
//! sit alongside the AUR fetch's rows in a single display.
//!
//! The downloaded DBs persist between runs (incremental `If-Modified-Since`
//! fetches), and [`synced_db_path`] hands them to the upgrade-check readers
//! ([`crate::pacman::invoke::query_repo_upgrades`],
//! [`crate::build::collect_upgrade_plan`]).
//!
//! [`update`]: alpm::AlpmList::update

use crate::error::{Error, Result};
use crate::pacman::alpm_db;
use crate::paths;
use crate::ui;
use alpm::{AnyDownloadEvent, DownloadEvent};
use indicatif::{MultiProgress, ProgressBar};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, instrument};

/// Outcome of a [`refresh_sync_db`] run, reported once the shared progress
/// display is torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// At least one repo DB advanced to a newer copy.
    Refreshed,
    /// Every repo DB was already current (`If-Modified-Since` 304s).
    AlreadyCurrent,
}

/// Refresh the official-repo sync DBs into gitaur's private dbpath, rootless.
///
/// Opens a mutable alpm handle at [`paths::sync_db_path`], wires a per-repo
/// download UI into `mp`, and runs [`alpm::AlpmList::update`] over every
/// registered sync DB. The handle is built and used entirely on the calling
/// thread (alpm is `!Sync`); only the shared [`MultiProgress`] crosses threads,
/// and that is safe.
///
/// Errors (unreadable `pacman.conf`, a download/verify failure, …) are returned
/// for the caller to downgrade to a warning — a repo-sync failure must never
/// fail the AUR refresh it runs beside.
#[instrument(skip(mp))]
pub fn refresh_sync_db(mp: &MultiProgress) -> Result<SyncOutcome> {
    let db = paths::sync_db_path();
    prepare_db_dir(&db)?;

    let mut alpm = alpm_db::open_at_for_refresh(&db)?;
    // Route alpm's per-file download events to indicatif rows. `DlProgress` is
    // moved into the handle as the callback's user data and lives until the
    // handle drops at the end of this function.
    alpm.set_dl_cb(DlProgress::new(mp.clone()), DlProgress::on_event);

    debug!(dbpath = %db.display(), "updating sync dbs (rootless)");
    // `update` wraps `alpm_db_update`, which returns 1 when *all* DBs were
    // already current and 0 when at least one was refreshed — so the bool is
    // "everything up to date", not "something changed".
    let all_current = alpm
        .syncdbs_mut()
        .update(false)
        .map_err(|e| Error::other(format!("sync db update: {e}")))?;

    Ok(if all_current {
        SyncOutcome::AlreadyCurrent
    } else {
        SyncOutcome::Refreshed
    })
}

/// gitaur's private dbpath, but only once it's actually usable — at least one
/// downloaded `*.db` under `sync/` and a `local` symlink that still resolves.
///
/// Returning `None` until both hold is load-bearing: an empty or half-built
/// store would report *every* installed package as foreign (no sync repo
/// declares it), so the upgrade-check readers fall back to the system dbpath
/// until the first successful [`refresh_sync_db`].
pub fn synced_db_path() -> Option<PathBuf> {
    let db = paths::sync_db_path();
    // `exists()` follows the symlink, so a dangling `local` reads as absent.
    if !db.join("local").exists() {
        return None;
    }
    let has_db = match std::fs::read_dir(db.join("sync")) {
        Ok(entries) => entries
            .flatten()
            .any(|e| e.path().extension().is_some_and(|ext| ext == "db")),
        Err(_) => false,
    };
    has_db.then_some(db)
}

/// Create the private dbpath and point its `local` at the system localdb.
///
/// pacman/libalpm create `sync/` themselves on first update; we only need the
/// dbpath root plus the `local` symlink so alpm reads the real installed set.
fn prepare_db_dir(db: &Path) -> Result<()> {
    std::fs::create_dir_all(db)?;
    let system_local = alpm_db::system_db_path()?.join("local");
    ensure_symlink(&system_local, &db.join("local"))
}

/// Idempotently make `link` a symlink to `target`. Re-points a link aimed
/// elsewhere (e.g. the system dbpath moved) and clears a non-symlink sitting in
/// the way; a no-op when it already points where we want.
fn ensure_symlink(target: &Path, link: &Path) -> Result<()> {
    match std::fs::read_link(link) {
        Ok(current) if current == target => return Ok(()),
        Ok(_) => std::fs::remove_file(link)?,
        // Not a symlink but something is there — clear it so we can relink.
        Err(_) if link.exists() => std::fs::remove_file(link)?,
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

/// Renders libalpm's per-file download events as one indicatif byte-row per
/// repo DB, in the shared [`MultiProgress`] so they line up with the AUR fetch
/// rows. Moved into the alpm handle as `set_dl_cb` user data; bars are cleared
/// as each file completes, with the caller's `mp.clear()` as a final backstop.
struct DlProgress {
    multi: MultiProgress,
    /// Live bars keyed by alpm's download filename (e.g. `core.db`).
    bars: HashMap<String, ProgressBar>,
}

impl DlProgress {
    fn new(multi: MultiProgress) -> Self {
        Self {
            multi,
            bars: HashMap::new(),
        }
    }

    /// `set_dl_cb` callback. alpm fires `Init` → `Progress`* → `Completed` per
    /// file; we only surface the repo DBs themselves, not their detached
    /// signatures (`*.db.sig`) which are tiny and would just add noise.
    ///
    /// `event` is taken by value because that's the shape libalpm's callback ABI
    /// dictates (`FnMut(&str, AnyDownloadEvent, &mut T)`); it isn't consumed.
    #[allow(clippy::needless_pass_by_value)]
    fn on_event(filename: &str, event: AnyDownloadEvent<'_>, this: &mut Self) {
        if Path::new(filename)
            .extension()
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("db"))
        {
            return;
        }
        match event.event() {
            DownloadEvent::Init(_) => {
                this.bar_for(filename);
            }
            DownloadEvent::Progress(p) => {
                let bar = this.bar_for(filename);
                let total = u64::try_from(p.total).unwrap_or(0);
                if total > 0 {
                    ui::promote_byte_bar(bar, total);
                }
                bar.set_position(u64::try_from(p.downloaded).unwrap_or(0));
            }
            DownloadEvent::Completed(_) => {
                if let Some(bar) = this.bars.remove(filename) {
                    bar.finish_and_clear();
                }
            }
            DownloadEvent::Retry(_) => {}
        }
    }

    /// The bar for `filename`, lazily created (and added to the shared
    /// `MultiProgress`) on first sighting.
    fn bar_for(&mut self, filename: &str) -> &ProgressBar {
        self.bars.entry(filename.to_owned()).or_insert_with(|| {
            let bar = self.multi.add(ui::bar_bytes_streaming(filename));
            ui::tick(&bar);
            bar
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure_symlink, synced_db_path};
    use crate::paths;
    use crate::testing::ScopedStateRoot;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn ensure_symlink_creates_retargets_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("target-a");
        let b = dir.path().join("target-b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let link = dir.path().join("local");

        // Created from nothing.
        ensure_symlink(&a, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), a);
        // Idempotent — already correct, left as-is.
        ensure_symlink(&a, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), a);
        // Re-pointed when the target changes.
        ensure_symlink(&b, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), b);
    }

    #[test]
    fn ensure_symlink_replaces_a_plain_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("local");
        // A regular file squatting where the symlink should go.
        fs::write(&link, b"stale").unwrap();

        ensure_symlink(&target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }

    #[test]
    fn synced_db_path_requires_local_link_and_a_sync_db() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let db = paths::sync_db_path();
        let sync = db.join("sync");
        fs::create_dir_all(&sync).unwrap();

        // A `*.db` but no `local` link → fall back to the system db.
        fs::write(sync.join("core.db"), b"x").unwrap();
        assert!(synced_db_path().is_none());

        // `local` present but no `*.db` → still incomplete.
        fs::remove_file(sync.join("core.db")).unwrap();
        let real_local = dir.path().join("real-local");
        fs::create_dir(&real_local).unwrap();
        std::os::unix::fs::symlink(&real_local, db.join("local")).unwrap();
        assert!(synced_db_path().is_none());

        // Both present → the private dbpath is usable.
        fs::write(sync.join("extra.db"), b"x").unwrap();
        assert_eq!(synced_db_path(), Some(db));
    }

    #[test]
    fn synced_db_path_rejects_a_dangling_local_link() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let db = paths::sync_db_path();
        let sync = db.join("sync");
        fs::create_dir_all(&sync).unwrap();
        fs::write(sync.join("core.db"), b"x").unwrap();
        // Points at a path that doesn't exist — `exists()` follows it and reads
        // false, so this dbpath must not be handed out.
        std::os::unix::fs::symlink(dir.path().join("gone"), db.join("local")).unwrap();
        assert!(synced_db_path().is_none());
    }
}
