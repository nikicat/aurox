//! Cross-session build-time history backing the change-set preview's build-time
//! column.
//!
//! Only build duration + when the build happened is persisted — every other
//! cost figure (download size, installed footprint) is already in pacman's
//! local/sync DBs. Schema is one append-only row per successful build:
//! `(pkgbase, build_secs, built_at_ms)`. We deliberately do *not* upsert —
//! keeping the full history is what makes future aggregation (median,
//! recent-weighted mean, drift detection) cheap; for now the read API just
//! returns the latest row's duration, which is the best single-shot predictor
//! of the next build. Timestamps are Unix epoch *milliseconds* so two builds in
//! the same wall second are still ordered; ROWID DESC is the secondary
//! tie-breaker (matches insert order regardless of clock).
//!
//! Errors propagate as [`crate::error::Error::Other`]. The build pipeline
//! downgrades any store error to a `warn!`: a failed metrics write must not
//! turn a successful build into a failure — the package is on disk and was
//! just installed, only the cost-visibility hint is lost.

use crate::error::{Error, Result};
use crate::names::PkgBase;
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, ToSql, params};
use std::borrow::Borrow;
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, instrument};

/// Bind a `PkgBase` as a SQL TEXT parameter. Lives here (not in `names.rs`) so
/// the rusqlite coupling stays in the one module that talks to SQL — `names.rs`
/// keeps its general-purpose trait impls only. Borrowed output: no allocation,
/// just an `&str` view through the wrapper's `Borrow<str>` impl.
impl ToSql for PkgBase {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            Borrow::<str>::borrow(self).as_bytes(),
        )))
    }
}

/// Owns the `SQLite` connection to `state_dir()/metrics.db`.
///
/// One connection per store — every call site is the upgrade loop
/// (single-threaded; builds run serially), so there's nothing to share or pool.
/// The connection is closed when the store is dropped.
pub struct MetricsStore {
    conn: Connection,
}

impl MetricsStore {
    /// Open (or create) the metrics DB at `path`, creating the parent
    /// directory as needed (fresh installs may not have `state_dir()` yet).
    ///
    /// Idempotent: re-opening an existing DB is a no-op for the schema/index
    /// statements.
    #[instrument]
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::other(format!("create metrics.db parent: {e}")))?;
        }
        let conn =
            Connection::open(path).map_err(|e| Error::other(format!("open metrics.db: {e}")))?;
        // Append-only history: many rows per pkgbase, ordered by `built_at_ms`.
        // No PRIMARY KEY on pkgbase — that would force upsert and lose the
        // history we want for future aggregation. SQLite supplies the rowid.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS build_metrics (
                 pkgbase     TEXT NOT NULL,
                 build_secs  INTEGER NOT NULL,
                 built_at_ms INTEGER NOT NULL
             )",
            [],
        )
        .map_err(|e| Error::other(format!("create build_metrics: {e}")))?;
        // Read path: latest row per pkgbase. The (pkgbase, built_at_ms DESC)
        // index turns the `ORDER BY built_at_ms DESC, ROWID DESC LIMIT 1` lookup
        // into a single index seek.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_build_metrics_pkgbase_built_at \
             ON build_metrics(pkgbase, built_at_ms DESC)",
            [],
        )
        .map_err(|e| Error::other(format!("create build_metrics index: {e}")))?;
        debug!("metrics store opened");
        Ok(Self { conn })
    }

    /// Append one successful build's `(duration, wall-clock time)`. Every call
    /// adds a row; we never overwrite. `built_at_ms` is recorded at call time
    /// as Unix epoch milliseconds — two builds in the same wall second stay
    /// ordered.
    #[instrument(skip(self), fields(pkgbase = %pkgbase))]
    pub fn record_build(&self, pkgbase: &PkgBase, build_secs: u64) -> Result<()> {
        // Floor stray > 2^63 values at i64::MAX rather than wrapping — a
        // 290-year build doesn't exist, but surface the cap if it ever does.
        let secs = i64::try_from(build_secs).unwrap_or(i64::MAX);
        let built_at_ms = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| Error::other(format!("clock before epoch: {e}")))?
                .as_millis(),
        )
        .unwrap_or(i64::MAX);
        self.conn
            .execute(
                "INSERT INTO build_metrics(pkgbase, build_secs, built_at_ms) VALUES (?1, ?2, ?3)",
                params![pkgbase, secs, built_at_ms],
            )
            .map_err(|e| Error::other(format!("insert build_metrics: {e}")))?;
        Ok(())
    }

    /// Most recent recorded build's `(duration_secs, built_at_ms)` for
    /// `pkgbase`, or `None` when no build of it has ever been recorded. The
    /// latest row beats older ones — build flows change (compiler upgrades,
    /// ccache warmth, parallelism tweaks), so the freshest figure is the best
    /// single predictor. `ROWID DESC` is the tie-breaker for back-to-back
    /// inserts that share a millisecond (or for backfilled rows with identical
    /// timestamps): insert order is the natural fallback for "latest".
    ///
    /// Returning the timestamp alongside the duration lets the change-set
    /// preview dim cells whose underlying measurement is stale — see
    /// [`BuildRecord::age`].
    pub fn latest_build(&self, pkgbase: &PkgBase) -> Result<Option<BuildRecord>> {
        self.conn
            .query_row(
                "SELECT build_secs, built_at_ms FROM build_metrics WHERE pkgbase = ?1 \
                 ORDER BY built_at_ms DESC, ROWID DESC LIMIT 1",
                params![pkgbase],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| Error::other(format!("select latest build_metrics: {e}")))
            .map(|opt| opt.map(|(secs, ts)| BuildRecord::clamp(secs, ts)))
    }

    /// Bulk latest-row lookup for a whole change-set preview — one prepared
    /// statement, one round-trip per pkgbase, rather than reopening the
    /// statement each time. Pkgbases with no recorded measurement are absent
    /// from the returned map (callers render `~?`).
    #[instrument(skip(self, pkgbases))]
    pub fn latest_build_many<'a, I>(&self, pkgbases: I) -> Result<HashMap<PkgBase, BuildRecord>>
    where
        I: IntoIterator<Item = &'a PkgBase>,
    {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT build_secs, built_at_ms FROM build_metrics WHERE pkgbase = ?1 \
                 ORDER BY built_at_ms DESC, ROWID DESC LIMIT 1",
            )
            .map_err(|e| Error::other(format!("prepare build_metrics lookup: {e}")))?;
        let mut out = HashMap::new();
        for pb in pkgbases {
            let row: Option<(i64, i64)> = stmt
                .query_row(params![pb], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .map_err(|e| Error::other(format!("query build_metrics: {e}")))?;
            if let Some((secs, ts)) = row {
                out.insert(pb.clone(), BuildRecord::clamp(secs, ts));
            }
        }
        Ok(out)
    }
}

/// One latest-row read from the store: duration plus when it was measured.
///
/// `built_at_ms` is the Unix-epoch millisecond timestamp the row was inserted
/// with; [`Self::age`] computes how stale that measurement is against `now`
/// (saturating at zero for forward-clock jitter so a row dated slightly in
/// the future isn't reported as a negative age).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildRecord {
    pub build_secs: u64,
    pub built_at_ms: i64,
}

impl BuildRecord {
    /// Floor stray negatives at 0 / `i64::MIN` so the constructor never panics
    /// on a hand-edited row carrying nonsense. Real inserts go through
    /// `record_build` and can't produce them.
    fn clamp(secs: i64, ts_ms: i64) -> Self {
        Self {
            build_secs: u64::try_from(secs).unwrap_or(0),
            built_at_ms: ts_ms,
        }
    }

    /// How long ago this measurement was recorded, in seconds. Saturates at 0
    /// when `now` is earlier than `built_at_ms` (clock skew, restored backup,
    /// row from another machine) so callers never see a negative age.
    pub fn age(self, now: SystemTime) -> Result<u64> {
        let now_ms = i64::try_from(
            now.duration_since(UNIX_EPOCH)
                .map_err(|e| Error::other(format!("clock before epoch: {e}")))?
                .as_millis(),
        )
        .unwrap_or(i64::MAX);
        let delta_ms = now_ms.saturating_sub(self.built_at_ms).max(0);
        Ok(u64::try_from(delta_ms / 1000).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, MetricsStore) {
        let dir = TempDir::new().unwrap();
        let store = MetricsStore::open(&dir.path().join("metrics.db")).unwrap();
        (dir, store)
    }

    /// Single-row count helper — keeps tests insulated from the schema; what
    /// matters is "did we add a row?", not which columns it has.
    fn row_count(store: &MetricsStore, pkgbase: &PkgBase) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM build_metrics WHERE pkgbase = ?1",
                params![pkgbase],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn missing_pkgbase_returns_none() {
        let (_dir, store) = fresh();
        assert_eq!(store.latest_build(&PkgBase::from("ghost")).unwrap(), None);
    }

    /// One insert round-trips: the latest read sees the value we stored with
    /// a recent timestamp, and the table holds exactly one row.
    #[test]
    fn record_then_read_round_trips() {
        let (_dir, store) = fresh();
        let pb = PkgBase::from("firefox-git");
        store.record_build(&pb, 4_321).unwrap();
        let rec = store.latest_build(&pb).unwrap().expect("inserted row");
        assert_eq!(rec.build_secs, 4_321);
        assert_eq!(row_count(&store, &pb), 1);
        // Sanity: built_at_ms must lie within the last few seconds.
        let age = rec.age(SystemTime::now()).unwrap();
        assert!(age < 10, "fresh insert reported age {age}s");
    }

    /// Successive builds of the same pkgbase are *appended*, not overwritten —
    /// future aggregation (median, drift detection) depends on the history
    /// surviving. The read API returns the freshest row.
    #[test]
    fn record_build_appends_history() {
        let (_dir, store) = fresh();
        let pb = PkgBase::from("paru-bin");
        store.record_build(&pb, 100).unwrap();
        store.record_build(&pb, 250).unwrap();
        assert_eq!(
            row_count(&store, &pb),
            2,
            "history must accumulate, not upsert"
        );
        assert_eq!(
            store.latest_build(&pb).unwrap().unwrap().build_secs,
            250,
            "latest read must return the freshest row"
        );
    }

    /// Re-opening the same DB sees prior rows — the open path doesn't truncate
    /// or `CREATE TABLE` over existing data.
    #[test]
    fn reopening_preserves_history() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metrics.db");
        {
            let s = MetricsStore::open(&path).unwrap();
            s.record_build(&PkgBase::from("cuda"), 9_000).unwrap();
            s.record_build(&PkgBase::from("cuda"), 10_000).unwrap();
        }
        let s = MetricsStore::open(&path).unwrap();
        assert_eq!(row_count(&s, &PkgBase::from("cuda")), 2);
        assert_eq!(
            s.latest_build(&PkgBase::from("cuda"))
                .unwrap()
                .unwrap()
                .build_secs,
            10_000
        );
    }

    /// Bulk lookup returns only recorded pkgbases; absent ones drop silently
    /// so the caller renders `~?` for them.
    #[test]
    fn latest_many_returns_only_recorded() {
        let (_dir, store) = fresh();
        let known = PkgBase::from("yay-bin");
        let unknown = PkgBase::from("never-built");
        store.record_build(&known, 42).unwrap();
        let got = store.latest_build_many([&known, &unknown]).unwrap();
        assert_eq!(got.get(&known).map(|r| r.build_secs), Some(42));
        assert!(!got.contains_key(&unknown));
    }

    /// Age computation: a row dated one hour ago reports ~3600 s; a row in
    /// the future (clock skew) saturates at 0 instead of underflowing.
    #[test]
    fn build_record_age_saturates_on_future_timestamps() {
        let now = SystemTime::now();
        let now_ms = i64::try_from(now.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
        let past = BuildRecord {
            build_secs: 100,
            built_at_ms: now_ms - 3_600_000,
        };
        let future = BuildRecord {
            build_secs: 100,
            built_at_ms: now_ms + 60_000,
        };
        let past_age = past.age(now).unwrap();
        assert!(
            (3590..=3610).contains(&past_age),
            "expected ~3600s ago, got {past_age}"
        );
        assert_eq!(
            future.age(now).unwrap(),
            0,
            "future timestamp must clamp to 0, not wrap"
        );
    }
}
