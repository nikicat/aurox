//! Cross-session PKGBUILD review approvals — the persistent half of the
//! reviewed set the shell and the build pipeline thread per session.
//!
//! One row per *consented* approval, keyed by `(pkgbase, commit)` where
//! `commit` is the mirror commit whose tree the user actually reviewed. The
//! commit is the concrete identity of what was approved: any new commit —
//! even one that keeps the same `pkgver` (a VCS pkgbase, a metadata-only
//! push) — re-prompts, while re-encountering the identical content never
//! does. The declared version rides along per row purely as observational
//! context (which release that commit built); it is never part of the key
//! and never consulted.
//!
//! Only decisions the user made at a decision point are recorded: a diff
//! answered at the review prompt, or an explicit `approve` command. The
//! auto-approved remainder of an "approve all" pass, `--noconfirm` runs, and
//! `aur_approval = "auto"` staging never write here — persisting those would
//! silently suppress future *interactive* reviews of content nobody looked
//! at.
//!
//! Errors propagate as [`crate::error::Error::Other`]; callers degrade them
//! to a warning — a broken sidecar DB must never block a build or lose an
//! apply (the same contract as [`super::metrics`]).

use crate::error::{Error, Result};
use crate::names::PkgBase;
use crate::version::Ver;
use gix::ObjectId;
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, instrument};

/// Owns the `SQLite` connection to `state_dir()/reviews.db`.
///
/// One connection per store; every caller is single-threaded (the shell's
/// dispatch loop, the serial build pipeline). Closed on drop.
pub struct ReviewStore {
    conn: Connection,
}

impl ReviewStore {
    /// Open (or create) the approvals DB at `path`, creating the parent
    /// directory as needed. Idempotent — re-opening an existing DB is a no-op
    /// for the schema statement.
    #[instrument]
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::other(format!("create reviews.db parent: {e}")))?;
        }
        let conn =
            Connection::open(path).map_err(|e| Error::other(format!("open reviews.db: {e}")))?;
        // The (pkgbase, commit) pair IS the fact; duplicates carry nothing, so
        // the primary key doubles as the lookup index and inserts are
        // OR IGNORE. `version`/`approved_at_ms` are per-row context only.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS review_approvals (
                 pkgbase        TEXT NOT NULL,
                 commit_oid     TEXT NOT NULL,
                 version        TEXT NOT NULL,
                 approved_at_ms INTEGER NOT NULL,
                 PRIMARY KEY (pkgbase, commit_oid)
             ) WITHOUT ROWID",
            [],
        )
        .map_err(|e| Error::other(format!("create review_approvals: {e}")))?;
        debug!("review store opened");
        Ok(Self { conn })
    }

    /// Record one consented approval of `pkgbase` at `commit` (declaring
    /// `version`). Re-recording a known pair keeps the original row — the
    /// first approval's timestamp is when consent happened.
    #[instrument(skip(self), fields(pkgbase = %pkgbase, commit = %commit))]
    pub fn record_approval(
        &self,
        pkgbase: &PkgBase,
        commit: &ObjectId,
        version: &Ver,
    ) -> Result<()> {
        let approved_at_ms = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| Error::other(format!("clock before epoch: {e}")))?
                .as_millis(),
        )
        .unwrap_or(i64::MAX);
        self.conn
            .execute(
                "INSERT OR IGNORE INTO review_approvals(pkgbase, commit_oid, version, approved_at_ms) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![pkgbase, commit.to_string(), version.as_str(), approved_at_ms],
            )
            .map_err(|e| Error::other(format!("insert review_approvals: {e}")))?;
        Ok(())
    }

    /// Whether `pkgbase` was approved at exactly `commit` in some prior
    /// (or this) session.
    pub fn approved(&self, pkgbase: &PkgBase, commit: &ObjectId) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM review_approvals WHERE pkgbase = ?1 AND commit_oid = ?2",
                params![pkgbase, commit.to_string()],
                |_| Ok(()),
            )
            .map(|()| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(Error::other(format!("select review_approvals: {other}"))),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::hash::Kind;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, ReviewStore) {
        let dir = TempDir::new().unwrap();
        let store = ReviewStore::open(&dir.path().join("reviews.db")).unwrap();
        (dir, store)
    }

    /// A recognizable non-null OID fixture: `byte` repeated over all 20 bytes.
    fn oid(byte: u8) -> ObjectId {
        ObjectId::from([byte; 20])
    }

    #[test]
    fn missing_pair_is_not_approved() {
        let (_dir, store) = fresh();
        assert!(!store.approved(&PkgBase::from("ghost"), &oid(1)).unwrap());
    }

    /// One approval round-trips, and is scoped to its exact pkgbase + commit:
    /// the same pkgbase at another commit (a new AUR push) still needs review,
    /// as does another pkgbase at the same commit value.
    #[test]
    fn approval_is_keyed_by_pkgbase_and_commit() {
        let (_dir, store) = fresh();
        let pb = PkgBase::from("yay-bin");
        store
            .record_approval(&pb, &oid(1), Ver::new("12.0-1"))
            .unwrap();
        assert!(store.approved(&pb, &oid(1)).unwrap());
        assert!(
            !store.approved(&pb, &oid(2)).unwrap(),
            "a new commit of the same pkgbase must re-review"
        );
        assert!(
            !store.approved(&PkgBase::from("paru-bin"), &oid(1)).unwrap(),
            "another pkgbase never inherits an approval"
        );
    }

    /// Re-recording the same pair is a quiet no-op (`INSERT OR IGNORE`) — the
    /// original consent row survives.
    #[test]
    fn re_recording_keeps_one_row() {
        let (_dir, store) = fresh();
        let pb = PkgBase::from("yay-bin");
        store
            .record_approval(&pb, &oid(1), Ver::new("12.0-1"))
            .unwrap();
        store
            .record_approval(&pb, &oid(1), Ver::new("12.0-1"))
            .unwrap();
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM review_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        assert!(store.approved(&pb, &oid(1)).unwrap());
    }

    /// Re-opening the same DB sees prior approvals — the whole point of the
    /// store (a new session inherits past consent).
    #[test]
    fn reopening_preserves_approvals() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("reviews.db");
        {
            let s = ReviewStore::open(&path).unwrap();
            s.record_approval(&PkgBase::from("cuda"), &oid(7), Ver::new("12.9.1-1"))
                .unwrap();
        }
        let s = ReviewStore::open(&path).unwrap();
        assert!(s.approved(&PkgBase::from("cuda"), &oid(7)).unwrap());
        assert!(!s.approved(&PkgBase::from("cuda"), &oid(8)).unwrap());
    }

    /// The null OID is a legal key like any other (an index entry that never
    /// recorded a commit can't collide with a real one).
    #[test]
    fn distinct_commits_never_alias() {
        let (_dir, store) = fresh();
        let pb = PkgBase::from("foo");
        store
            .record_approval(&pb, &ObjectId::null(Kind::Sha1), Ver::new("1-1"))
            .unwrap();
        assert!(store.approved(&pb, &ObjectId::null(Kind::Sha1)).unwrap());
        assert!(!store.approved(&pb, &oid(0x11)).unwrap());
    }
}
