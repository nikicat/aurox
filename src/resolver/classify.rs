//! Classify a single dep reference into Installed / Repo / AUR / Missing.

use crate::index::secondary::Secondary;
use crate::index::IndexFile;
use crate::pacman::alpm_db;
use alpm::Alpm;

/// Where a given dep name lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Already in the local pacman DB; nothing to do.
    Installed,
    /// Available in a sync repo; install via pacman batch.
    Repo,
    /// AUR pkgbase at `idx.entries[usize]`.
    Aur(usize),
    /// Could not be resolved anywhere.
    Missing,
}

/// Classify `name` (already stripped of any version constraint).
pub fn classify(_idx: &IndexFile, by: &Secondary, alpm: &Alpm, name: &str) -> Source {
    if alpm_db::installed_version(alpm, name).is_some() {
        return Source::Installed;
    }
    if alpm_db::syncdb_provides(alpm, name) {
        return Source::Repo;
    }
    if let Some(&i) = by.by_name.get(name) {
        return Source::Aur(i as usize);
    }
    if let Some(providers) = by.by_provides.get(name) {
        if let Some(&i) = providers.first() {
            return Source::Aur(i as usize);
        }
    }
    Source::Missing
}
