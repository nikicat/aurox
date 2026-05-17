//! Spawn `makepkg` with deterministic PKGDEST/SRCDEST/BUILDDIR placement.

use crate::config::Config;
use crate::error::{Error, Result};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, instrument};

/// Run `makepkg` in `worktree` with the configured args + env.
#[instrument(skip(cfg))]
pub fn run(cfg: &Config, worktree: &Path) -> Result<()> {
    let mut cmd = Command::new(&cfg.makepkg_path);
    cmd.current_dir(worktree)
        .env("PKGDEST", worktree)
        .env("SRCDEST", worktree.join("src-cache"))
        .env("BUILDDIR", worktree);
    cmd.args(&cfg.makepkg_args);
    debug!(args = ?cfg.makepkg_args, cwd = %worktree.display(), "spawning makepkg");

    let status = cmd.status()?;
    if !status.success() {
        let code = status.code().unwrap_or(1);
        return Err(Error::Build(format!("makepkg exited with status {code}")));
    }
    info!("makepkg succeeded");
    Ok(())
}
