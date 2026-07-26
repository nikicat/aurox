//! `-G` / `-Gp` — hand a pkgbase's AUR git repo to the user for hacking on.
//!
//! yay/paru clone from `https://aur.archlinux.org/<pkgbase>.git`, and AUR's
//! HTTPS endpoint is fetch-only: landing a patch means rewriting `origin` to
//! the SSH form by hand first. aurox already holds every pkgbase's full
//! history in the local mirror, so `-G` clones from *disk* — no network, no
//! RPC — and points `origin` straight at the pushable endpoint. Edit, commit,
//! push.
//!
//! `--no-local` on the clone is load-bearing. Git's local-path optimization
//! copies (hardlinking) the *whole* object store and only filters refs, which
//! for the AUR mirror means 9M objects / 2.7 GiB of apparent size in the
//! user's new directory. Forcing the git-aware transport packs exactly the
//! requested branch instead — ~250 KiB for a typical pkgbase, full history
//! included.
//!
//! **Why the two halves use different git implementations.** `-Gp` is a pure
//! read of an object aurox already holds, so it goes through `gix` like every
//! other blob read in the crate (`index/build.rs`, `index/update.rs`,
//! `index/info.rs`'s maintainer scan) — no subprocess, no output to parse. The
//! clone shells out because what it produces is a repository *the user's own
//! git works in next*: `git clone` is the reference implementation of the
//! index, `HEAD`, config and remote layout every later `git status` / `commit`
//! / `push` expects, and it lays all of that down in one call. The mirror's
//! own clone (`mirror/clone.rs`) is native `gix`, but it's a *bare* fetch with
//! a custom refspec — no worktree, no checkout — so none of it is reusable
//! here. Same split, same reason, as `mirror/worktree.rs`.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git;
use crate::index::AurIndexData;
use crate::mirror::MirrorRepo;
use crate::names::{PkgBase, PkgTarget};
use crate::paths;
use crate::ui;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use tracing::instrument;

/// The AUR's **pushable** git endpoint. Deliberately not
/// [`Config::mirror_url`](crate::config::Config::mirror_url): that names
/// whichever mirror aurox *reads* (the GitHub one by default), while a fix
/// travels back to the AUR itself, over SSH, always at this host.
const AUR_SSH_BASE: &str = "ssh://aur@aur.archlinux.org";

/// What `-G` does with each resolved pkgbase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GetMode {
    /// `-G`: clone the pkgbase's repo into `./<pkgbase>`.
    Clone,
    /// `-Gp`: print its PKGBUILD to stdout, touching no files.
    Print,
}

/// `aurox -G <pkg>…` / `aurox -Gp <pkg>…`.
///
/// Targets resolve the way `-Si`'s do (pkgname / provides / pkgbase), so
/// `-G <pkgname>` lands the split-package *base* that builds it. Pacman-style
/// exit code: non-zero when some target wasn't in the AUR.
#[instrument(skip(cfg))]
pub fn cmd_get(cfg: &Config, targets: &[PkgTarget], mode: GetMode) -> Result<u8> {
    if targets.is_empty() {
        return Err(Error::other("no targets specified"));
    }
    let data = AurIndexData::load(cfg)?;
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for target in targets {
        match data.entry(target) {
            Some(entry) => found.push(&entry.pkgbase),
            None => missing.push(target.to_string()),
        }
    }
    let mirror = paths::aur_repo_path();
    match mode {
        GetMode::Clone => {
            for pkgbase in found {
                clone_pkgbase(&mirror, pkgbase, Path::new(pkgbase.as_str()))?;
            }
        }
        GetMode::Print => {
            // Opened once for the run — the reads are plain object lookups.
            let repo = MirrorRepo::open(&mirror)?;
            for pkgbase in found {
                // Raw bytes: it's file content, not a rendered message.
                io::stdout().write_all(&pkgbuild_blob(&repo, pkgbase)?)?;
            }
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    ui::warn(&format!("not in the AUR: {}", missing.join(", ")));
    Ok(1)
}

/// Clone `pkgbase`'s branch out of the local `mirror` into `dest`, then point
/// `origin` at the AUR's SSH URL so `git push` works with no remote surgery.
///
/// A `dest` that already exists is git's complaint to make, not ours to
/// resolve — we never overwrite a directory the user may have work in.
fn clone_pkgbase(mirror: &Path, pkgbase: &PkgBase, dest: &Path) -> Result<()> {
    git::run(
        [
            "clone".as_ref(),
            "--no-local".as_ref(),
            "--single-branch".as_ref(),
            "--branch".as_ref(),
            OsStr::new(pkgbase.as_str()),
            "--".as_ref(),
            mirror.as_os_str(),
            dest.as_os_str(),
        ],
        None,
    )?;
    let remote = format!("{AUR_SSH_BASE}/{pkgbase}.git");
    git::run(
        [
            "-C".as_ref(),
            dest.as_os_str(),
            "remote".as_ref(),
            "set-url".as_ref(),
            "origin".as_ref(),
            OsStr::new(remote.as_str()),
        ],
        None,
    )?;
    ui::info(&format!("{} — push-ready ({remote})", dest.display()));
    Ok(())
}

/// `pkgbase`'s PKGBUILD as of the mirror's branch tip, verbatim — a PKGBUILD
/// is not guaranteed UTF-8, and `-Gp`'s output is meant to be redirectable
/// into a file that still builds.
fn pkgbuild_blob(mirror: &MirrorRepo, pkgbase: &PkgBase) -> Result<Vec<u8>> {
    let refname = format!("refs/heads/{pkgbase}");
    let tip = mirror
        .repo
        .find_reference(&refname)
        .map_err(|e| Error::gix(format_args!("find_reference {refname}"), e))?
        .peel_to_id()
        .map_err(|e| Error::gix(format_args!("peel {refname}"), e))?
        .detach();
    let tree = mirror
        .repo
        .find_commit(tip)
        .map_err(|e| Error::gix(format_args!("find commit {tip}"), e))?
        .tree()
        .map_err(|e| Error::gix(format_args!("tree of {tip}"), e))?;
    let entry = tree
        .find_entry("PKGBUILD")
        .ok_or_else(|| Error::other(format!("{pkgbase} has no PKGBUILD at its branch tip")))?;
    let blob = mirror
        .repo
        .find_object(entry.oid().to_owned())
        .map_err(|e| Error::gix(format_args!("find PKGBUILD blob of {pkgbase}"), e))?;
    Ok(blob.detach().data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_contains;
    use tempfile::TempDir;

    /// A stand-in mirror: one bare repo, one `refs/heads/<pkgbase>` branch
    /// carrying a PKGBUILD — the shape [`clone_pkgbase`] and
    /// [`pkgbuild_blob`] read. Per-repo identity + no signing so the host's
    /// global git config can't reach in.
    fn mirror_with(pkgbase: &PkgBase, pkgbuild: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        git::run(["init", "--bare", dir.path().to_str().unwrap()], None).unwrap();
        let wt = TempDir::new().unwrap();
        let w = Some(wt.path());
        git::run(["init", wt.path().to_str().unwrap()], None).unwrap();
        git::run(["config", "user.email", "t@t"], w).unwrap();
        git::run(["config", "user.name", "t"], w).unwrap();
        git::run(["config", "commit.gpgsign", "false"], w).unwrap();
        std::fs::write(wt.path().join("PKGBUILD"), pkgbuild).unwrap();
        git::run(["add", "PKGBUILD"], w).unwrap();
        git::run(["commit", "-m", "initial"], w).unwrap();
        git::run(
            [
                "push",
                dir.path().to_str().unwrap(),
                &format!("HEAD:refs/heads/{pkgbase}"),
            ],
            w,
        )
        .unwrap();
        dir
    }

    #[test]
    fn clone_lands_the_files_with_a_pushable_origin() {
        let pkgbase = PkgBase::from("hello-bin");
        let mirror = mirror_with(&pkgbase, "pkgname=hello-bin\n");
        let out = TempDir::new().unwrap();
        let dest = out.path().join("hello-bin");

        clone_pkgbase(mirror.path(), &pkgbase, &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("PKGBUILD")).unwrap(),
            "pkgname=hello-bin\n"
        );
        let origin = git::run(["remote", "get-url", "origin"], Some(dest.as_path())).unwrap();
        assert_eq!(
            String::from_utf8(origin).unwrap().trim(),
            "ssh://aur@aur.archlinux.org/hello-bin.git",
            "origin must be the pushable AUR endpoint, not the mirror we cloned from"
        );
        // History travels with the copy — that's the point of a clone over a
        // file copy: the user can `git log`, amend, and push a real commit.
        let log = git::run(["log", "--oneline"], Some(dest.as_path())).unwrap();
        assert_contains!(String::from_utf8(log).unwrap(), "initial");
    }

    #[test]
    fn blob_reads_the_branch_tip_pkgbuild() {
        let pkgbase = PkgBase::from("hello-bin");
        let mirror = mirror_with(&pkgbase, "pkgname=hello-bin\npkgver=1.2.3\n");
        let repo = MirrorRepo::open(mirror.path()).unwrap();
        let blob = pkgbuild_blob(&repo, &pkgbase).unwrap();
        assert_contains!(String::from_utf8(blob).unwrap(), "pkgver=1.2.3");
    }

    /// An index entry whose branch the mirror doesn't carry (stale index,
    /// pruned branch) must say *which* ref was missing, not fail anonymously.
    #[test]
    fn blob_error_names_the_missing_branch() {
        let mirror = mirror_with(&PkgBase::from("hello-bin"), "pkgname=hello-bin\n");
        let repo = MirrorRepo::open(mirror.path()).unwrap();
        let err = pkgbuild_blob(&repo, &PkgBase::from("not-a-branch"))
            .unwrap_err()
            .to_string();
        assert_contains!(err, "refs/heads/not-a-branch");
    }
}
