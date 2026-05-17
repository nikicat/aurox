//! Incremental fetch of the AUR mirror via gix.
//!
//! Returns a per-ref delta vector ([`RefUpdate`]) compatible with the rest of
//! the index pipeline.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror::MirrorRepo;
use crate::ui::GixProgress;
use gix::bstr::{BString, ByteSlice};
use gix::remote::{ref_map::Options as RefMapOptions, Direction};
use gix::ObjectId;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tracing::{debug, info, instrument};

/// One refname change reported by the fetch.
#[derive(Debug, Clone)]
pub struct RefUpdate {
    /// Branch name (without `refs/heads/`).
    pub refname: String,
    /// Previous tip; `None` if the ref was newly created.
    pub old_oid: Option<ObjectId>,
    /// New tip; `None` if the ref was deleted.
    pub new_oid: Option<ObjectId>,
}

/// Fetch `refs/heads/*` from the mirror remote and collect [`RefUpdate`]s.
#[instrument(skip(_cfg, mirror))]
pub fn incremental_fetch(_cfg: &Config, mirror: &MirrorRepo) -> Result<Vec<RefUpdate>> {
    let mut progress = GixProgress::new("fetch");
    let interrupt = AtomicBool::new(false);

    let outcome = {
        let remote = mirror
            .repo
            .find_default_remote(Direction::Fetch)
            .ok_or_else(|| Error::Gix("no default remote configured".into()))?
            .map_err(|e| Error::Gix(format!("find_default_remote: {e}")))?;

        let connection = remote
            .connect(Direction::Fetch)
            .map_err(|e| Error::Gix(format!("connect: {e}")))?;

        debug!("preparing fetch: handshake + list refs against remote");
        let t_prepare = Instant::now();
        let prepared = connection
            .prepare_fetch(&mut progress, RefMapOptions::default())
            .map_err(|e| Error::Gix(format!("prepare_fetch: {e}")))?;
        debug!(
            elapsed_ms = u64::try_from(t_prepare.elapsed().as_millis()).unwrap_or(u64::MAX),
            "prepare_fetch returned (ref advertisement complete)"
        );

        // The next ~30–60s on a large mirror are gix-internal and silent:
        //   1. build local "have" set from existing refs (silent ~20s on AUR)
        //   2. negotiate (visible — `set_name=negotiate (round N)`)
        //   3. receive + index pack (visible — `read pack`, `create index file`)
        //   4. update refs / write pack manifest (silent ~15s on AUR)
        // We bracket the whole thing with start/end logs so the silent gaps
        // have context even when no gix progress event is firing.
        debug!("entering receive: build have-set, negotiate, fetch pack, update refs");
        let t_receive = Instant::now();
        let outcome = prepared
            .receive(&mut progress, &interrupt)
            .map_err(|e| Error::Gix(format!("receive: {e}")))?;
        debug!(
            elapsed_ms = u64::try_from(t_receive.elapsed().as_millis()).unwrap_or(u64::MAX),
            "receive returned (pack written, refs negotiated)"
        );
        outcome
    };

    progress.finish();

    debug!(
        mappings = outcome.ref_map.mappings.len(),
        "computing ref deltas"
    );

    let t_filter = Instant::now();
    let candidates: Vec<(String, Option<ObjectId>, BString)> = outcome
        .ref_map
        .mappings
        .iter()
        .filter_map(|m| {
            let refname = m.remote.as_name().map(|n| n.to_str_lossy().into_owned())?;
            if !refname.starts_with("refs/heads/") {
                return None;
            }
            let new_oid = m.remote.as_id().map(std::borrow::ToOwned::to_owned);
            let local = m.local.as_ref()?.clone();
            Some((refname, new_oid, local))
        })
        .collect();
    debug!(
        candidates = candidates.len(),
        elapsed_ms = u64::try_from(t_filter.elapsed().as_millis()).unwrap_or(u64::MAX),
        "filtered branch refs"
    );

    debug!(
        candidates = candidates.len(),
        "resolving local tips (one repo lookup per candidate, disk-bound)"
    );
    let t_resolve = Instant::now();
    let mut updates = Vec::new();
    for (refname, new_oid, local) in candidates {
        let old_oid = mirror
            .repo
            .find_reference(local.as_bstr())
            .ok()
            .and_then(|r| r.target().try_id().map(std::borrow::ToOwned::to_owned));
        if old_oid != new_oid {
            debug!(refname = %refname, ?old_oid, ?new_oid, "ref delta");
            updates.push(RefUpdate {
                refname,
                old_oid,
                new_oid,
            });
        }
    }
    debug!(
        updates = updates.len(),
        elapsed_ms = u64::try_from(t_resolve.elapsed().as_millis()).unwrap_or(u64::MAX),
        "resolved local tips"
    );

    info!(count = updates.len(), "fetch complete");
    Ok(updates)
}
