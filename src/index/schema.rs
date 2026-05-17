//! On-disk index schema. Persisted via `rkyv 0.8` zero-copy archive.

use rkyv::{Archive, Deserialize, Serialize};

/// One pkgbase row. Split-package pkgnames are all listed in `pkgnames`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
pub struct IndexEntry {
    /// Pkgbase (also the branch name on the mirror).
    pub pkgbase: String,
    /// All pkgnames produced by this pkgbase (single entry for non-split pkgs).
    pub pkgnames: Vec<String>,
    /// `pkgver` field.
    pub pkgver: String,
    /// `pkgrel` field.
    pub pkgrel: String,
    /// Optional `epoch` field (often unset).
    pub epoch: Option<String>,
    /// One-line description (`pkgdesc`).
    pub pkgdesc: Option<String>,
    /// Runtime dependencies.
    pub depends: Vec<String>,
    /// Build-time dependencies.
    pub makedepends: Vec<String>,
    /// Test/check dependencies.
    pub checkdepends: Vec<String>,
    /// Optional runtime dependencies (with `: reason` suffixes preserved).
    pub optdepends: Vec<String>,
    /// `provides` virtual names.
    pub provides: Vec<String>,
    /// `conflicts` declarations.
    pub conflicts: Vec<String>,
    /// `replaces` declarations.
    pub replaces: Vec<String>,
    /// Supported `arch` list.
    pub arch: Vec<String>,
    /// Commit OID of the branch tip that produced this entry.
    pub commit_oid: [u8; 20],
    /// Blob OID of the `.SRCINFO` file inside that commit's tree.
    pub srcinfo_blob_oid: [u8; 20],
}

/// Top-level archive: header metadata + entries sorted by `pkgbase`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
pub struct IndexFile {
    /// Format version, bumped on incompatible schema changes.
    pub format_version: u32,
    /// HEAD of the mirror at the time this index was written.
    pub mirror_head_oid: [u8; 20],
    /// Unix timestamp of last index write.
    pub built_at_unix: u64,
    /// Entries, sorted by pkgbase for stable diffs.
    pub entries: Vec<IndexEntry>,
}

impl IndexFile {
    /// Current format version constant.
    pub const FORMAT_VERSION: u32 = 1;

    /// Empty in-memory index. Used when no on-disk file exists yet.
    pub fn empty() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            mirror_head_oid: [0u8; 20],
            built_at_unix: 0,
            entries: Vec::new(),
        }
    }
}
