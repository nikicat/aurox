//! PKGBUILD review UX: label by installed-vs-new (install / upgrade / reinstall),
//! and on upgrade show a colored diff against the AUR commit whose `.SRCINFO`
//! declares the currently-installed version. Falls back to the full PKGBUILD
//! on fresh installs, reinstalls, and upgrades where no historic commit
//! matches (typical for VCS pkgbases whose `pkgver()` overrides the static
//! field at build time, or for installs older than the bounded history walk).
//!
//! Diff uses the bare mirror repo's object DB (not a `.git` inside the
//! worktree) — the build directory is just materialized files.

use crate::error::{Error, Result};
use crate::index::srcinfo;
use crate::mirror::worktree::Worktree;
use crate::mirror::MirrorRepo;
use crate::ui;
use dialoguer::Select;
use gix::ObjectId;
use std::process::Command;
use tracing::{debug, info, instrument};

/// How many commits back to scan looking for the AUR commit that produced
/// `installed_ver`. AUR maintainers bump versions one commit at a time, so
/// the match almost always sits in the first few commits. Bounded to keep
/// the walk cheap on a very stale install.
const MAX_HISTORY_SCAN: usize = 64;

/// What the user decided about this pkgbase. `Aborted` short-circuits the
/// whole pipeline (propagated as [`Error::UserAbort`] by the caller), so it
/// isn't a variant here — only "include it" vs "drop it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// User approved: include in the upcoming build batch.
    Approved,
    /// User chose "skip": drop this pkgbase from the build batch but keep
    /// reviewing the rest.
    Skipped,
}

/// Drive the review prompt loop for one pkgbase. `installed_ver` is the
/// pacman-localdb version of any pkgname in this pkgbase (None when not
/// installed); `new_ver` is the version the AUR index reports.
#[instrument(skip(mirror, wt))]
pub fn review(
    mirror: &MirrorRepo,
    pkgbase: &str,
    new_ver: &str,
    installed_ver: Option<&str>,
    wt: &Worktree,
    noconfirm: bool,
) -> Result<Outcome> {
    if noconfirm {
        info!(pkgbase, "auto-proceeding (noconfirm)");
        return Ok(Outcome::Approved);
    }

    loop {
        show(mirror, pkgbase, new_ver, installed_ver, wt)?;
        let choice = Select::new()
            .with_prompt(format!("[{pkgbase}] review"))
            .items(&["proceed", "view PKGBUILD", "edit", "skip", "abort"])
            .default(0)
            .interact()
            .map_err(|e| Error::other(format!("prompt: {e}")))?;
        match choice {
            0 => return Ok(Outcome::Approved),
            1 => show_pkgbuild(wt)?,
            2 => edit_pkgbuild(wt)?,
            3 => return Ok(Outcome::Skipped),
            _ => return Err(Error::UserAbort),
        }
    }
}

fn show(
    mirror: &MirrorRepo,
    pkgbase: &str,
    new_ver: &str,
    installed_ver: Option<&str>,
    wt: &Worktree,
) -> Result<()> {
    let header = match installed_ver {
        None => format!("install: {pkgbase} {new_ver}"),
        Some(v) if v == new_ver => format!("reinstall: {pkgbase} {new_ver}"),
        Some(v) => format!("upgrade: {pkgbase} {v} → {new_ver}"),
    };
    ui::step(&header);

    // Fresh install or reinstall: no historic version to diff against, so the
    // full PKGBUILD is the only meaningful review surface.
    let Some(installed) = installed_ver.filter(|v| *v != new_ver) else {
        return show_pkgbuild(wt);
    };

    if let Some(base) = find_installed_commit(mirror, wt.head_oid, installed)? {
        show_diff(mirror, wt, base)
    } else {
        ui::note(&format!(
            "no AUR commit in the last {MAX_HISTORY_SCAN} matches installed {installed}; showing full PKGBUILD"
        ));
        show_pkgbuild(wt)
    }
}

fn show_pkgbuild(wt: &Worktree) -> Result<()> {
    let text = std::fs::read_to_string(wt.path.join("PKGBUILD"))?;
    print!("{}", highlight::pkgbuild(&text));
    Ok(())
}

fn edit_pkgbuild(wt: &Worktree) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let pkgbuild = wt.path.join("PKGBUILD");
    debug!(editor, file = %pkgbuild.display(), "launching editor");
    let status = Command::new(editor).arg(&pkgbuild).status()?;
    if !status.success() {
        return Err(Error::Build(format!("editor exited {:?}", status.code())));
    }
    Ok(())
}

/// Show a line-diff of `PKGBUILD` between `base` and the freshly-materialized
/// worktree's commit. Delegates to the user's `git diff` (so their configured
/// pager / external differ — delta, diff-so-fancy, difftastic, etc. — kicks
/// in automatically when stdout is a TTY). Listing every other changed path
/// is left to the user — they have a real linked worktree where plain
/// `git diff` works.
fn show_diff(mirror: &MirrorRepo, wt: &Worktree, base: ObjectId) -> Result<()> {
    // `git diff` exits 0 when there are no differences and 1 when there are
    // — both are success. Any other status (or a spawn failure) is a real
    // error worth surfacing.
    let status = Command::new("git")
        .arg("-C")
        .arg(&mirror.path)
        .arg("diff")
        .arg(base.to_string())
        .arg(wt.head_oid.to_string())
        .args(["--", "PKGBUILD"])
        .status()
        .map_err(|e| Error::other(format!("spawn git diff: {e}")))?;
    match status.code() {
        Some(0 | 1) => Ok(()),
        Some(c) => Err(Error::other(format!("git diff exited {c}"))),
        None => Err(Error::other("git diff terminated by signal".to_string())),
    }
}

/// Walk the AUR branch back from `head_oid` looking for the commit whose
/// `.SRCINFO` declares `installed_ver`. Returns `None` if no such commit is
/// found within [`MAX_HISTORY_SCAN`] steps — VCS pkgbases never match here
/// because their static pkgver is overridden by `pkgver()` at build time,
/// and very stale installs may sit further back than the bound.
///
/// Uses `.SRCINFO` rather than parsing `PKGBUILD` ourselves: the AUR ships
/// the post-bash-expansion `.SRCINFO` alongside every PKGBUILD, and the
/// existing [`srcinfo::parse`] already turns it into an [`IndexEntry`] —
/// the same code path the rkyv index uses.
///
/// `pub` for integration tests (`tests/review_diff_history.rs`).
pub fn find_installed_commit(
    mirror: &MirrorRepo,
    head_oid: ObjectId,
    installed_ver: &str,
) -> Result<Option<ObjectId>> {
    let head = mirror
        .repo
        .find_commit(head_oid)
        .map_err(|e| Error::Gix(format!("find_commit {head_oid}: {e}")))?;
    let walk = head
        .ancestors()
        .first_parent_only()
        .all()
        .map_err(|e| Error::Gix(format!("ancestors {head_oid}: {e}")))?;
    for info in walk.take(MAX_HISTORY_SCAN) {
        let info = info.map_err(|e| Error::Gix(format!("walk: {e}")))?;
        let tree = info
            .object()
            .map_err(|e| Error::Gix(format!("walk object {}: {e}", info.id)))?
            .tree()
            .map_err(|e| Error::Gix(format!("walk tree {}: {e}", info.id)))?;
        let Some(text) = read_blob(mirror, &tree, ".SRCINFO")? else {
            continue;
        };
        let Ok(entry) = srcinfo::parse(&text) else {
            continue;
        };
        if entry.version() == installed_ver {
            return Ok(Some(info.id));
        }
    }
    Ok(None)
}

fn read_blob(mirror: &MirrorRepo, tree: &gix::Tree<'_>, name: &str) -> Result<Option<String>> {
    let Some(entry) = tree.find_entry(name) else {
        return Ok(None);
    };
    let oid = entry.oid().to_owned();
    let blob = mirror
        .repo
        .find_object(oid)
        .map_err(|e| Error::Gix(format!("find {name} blob: {e}")))?;
    Ok(Some(
        String::from_utf8_lossy(blob.data.as_slice()).into_owned(),
    ))
}

mod highlight {
    //! Bash syntax coloring for the PKGBUILD review screen, via `syntect`'s
    //! bundled Sublime grammar (same grammar `bat` uses for `.sh`/PKGBUILD).
    //!
    //! Loaded lazily — the bundled `SyntaxSet` costs ~100 ms to parse on first
    //! use, then is cached for the rest of the process. Any failure (theme
    //! missing, grammar unloadable, per-line highlight error) falls back to
    //! plain text rather than aborting review.
    use crate::ui;
    use std::sync::OnceLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::{Theme, ThemeSet};
    use syntect::parsing::SyntaxSet;
    use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

    struct Ctx {
        syntaxes: SyntaxSet,
        theme: Theme,
    }

    fn ctx() -> &'static Ctx {
        static CTX: OnceLock<Ctx> = OnceLock::new();
        CTX.get_or_init(|| Ctx {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: ThemeSet::load_defaults()
                .themes
                .remove("base16-ocean.dark")
                .expect("syntect ships base16-ocean.dark"),
        })
    }

    /// Render PKGBUILD source. Always ends with a single `\n` so the prompt
    /// that follows lands on a fresh line; passes `false` to the terminal
    /// escaper so the theme's background never paints over the user's bg.
    pub fn pkgbuild(text: &str) -> String {
        render(text, ui::color_on())
    }

    fn render(text: &str, colors: bool) -> String {
        if !colors {
            return plain(text);
        }
        try_color(text).unwrap_or_else(|| plain(text))
    }

    fn plain(text: &str) -> String {
        if text.is_empty() || text.ends_with('\n') {
            return text.to_owned();
        }
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(text);
        s.push('\n');
        s
    }

    fn try_color(text: &str) -> Option<String> {
        if text.is_empty() {
            return Some(String::new());
        }
        let Ctx { syntaxes, theme } = ctx();
        let syntax = syntaxes
            .find_syntax_by_name("Bourne Again Shell (bash)")
            .or_else(|| syntaxes.find_syntax_by_extension("sh"))?;
        let mut hl = HighlightLines::new(syntax, theme);
        let mut out = String::with_capacity(text.len() * 2);
        for line in LinesWithEndings::from(text) {
            let ranges = hl.highlight_line(line, syntaxes).ok()?;
            out.push_str(&as_24_bit_terminal_escaped(&ranges, false));
        }
        // Move any trailing newline past the reset so the styled block ends
        // with `\x1b[0m\n` regardless of whether the source had a final \n.
        if out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\u{1b}[0m\n");
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::render;
        use console::strip_ansi_codes;

        #[test]
        fn colored_roundtrips_to_source() {
            let src = "pkgname=foo\npkgver=1.2.3\n\nbuild() {\n    cd \"$srcdir/$pkgname-$pkgver\"  # comment\n    make\n}\n";
            let out = render(src, true);
            assert!(out.contains("\u{1b}["), "expected ANSI escapes: {out:?}");
            assert!(
                out.ends_with("\u{1b}[0m\n"),
                "missing final reset+nl: {out:?}"
            );
            // Strip the trailing reset before comparing, since strip_ansi_codes
            // leaves the surrounding text alone.
            assert_eq!(
                strip_ansi_codes(&out).trim_end_matches('\n'),
                src.trim_end_matches('\n')
            );
        }

        #[test]
        fn plain_when_colors_off() {
            let src = "pkgname=foo\n";
            assert_eq!(render(src, false), src);
        }

        #[test]
        fn adds_trailing_newline_when_source_lacks_one() {
            assert_eq!(render("pkgname=foo", false), "pkgname=foo\n");
            let out = render("pkgname=foo", true);
            assert!(out.ends_with("\u{1b}[0m\n"));
        }

        #[test]
        fn empty_input_stays_empty() {
            assert_eq!(render("", false), "");
            assert_eq!(render("", true), "");
        }

        #[test]
        fn utf8_in_pkgdesc_does_not_panic() {
            let src = "pkgdesc=\"héllo wörld — 漢字\"\n";
            let out = render(src, true);
            assert_eq!(
                strip_ansi_codes(&out).trim_end_matches('\n'),
                src.trim_end_matches('\n')
            );
        }
    }
}
