//! `gitaur <term>...` — yay-style fuzzy search → multi-select → install.
//!
//! Wired up from [`crate::cli::dispatch`] for the no-operation-letter case.
//! Picked pkgbases are routed through [`crate::build::cmd_install`].

use crate::build::{self, Target};
use crate::cli::Cli;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::{self, secondary::Secondary, IndexEntry};
use crate::paths;
use crate::ui;

use console::style;
use dialoguer::MultiSelect;
use std::io::IsTerminal;
use tracing::{debug, info, instrument};

/// Outcome of the picker step — distinguishes the three terminal states the
/// caller must dispatch on differently:
///   * `Listed` — non-interactive (no TTY or `--noconfirm`); the search hits
///     were printed to stdout, nothing to install. The caller returns `Ok(0)`
///     so `gitaur foo | head` is a legitimate "search" pipeline.
///   * `Picked` — interactive: the user kept at least one row. Caller routes
///     into `build::cmd_install`.
///   * `Aborted` — interactive: the user explicitly cleared every row. Caller
///     returns `Error::UserAbort` so scripts can detect the abort.
enum PickOutcome {
    Listed,
    Picked(Vec<String>),
    Aborted,
}

/// Entry point for the bare-positional shortcut.
///
/// `terms` are the freeform regex fragments the user typed; they're combined
/// as an AND filter (same semantics as `-Ss`). All matched pkgbases land in
/// a single picker so the user can pick across split-pkg families in one pass.
#[instrument(skip(cfg))]
pub fn cmd_search_install(cfg: &Config, cli: &Cli, terms: &[String]) -> Result<u8> {
    let noconfirm = cli.noconfirm;
    let asdeps = cli.asdeps;

    let path = paths::index_path();
    if !path.exists() {
        ui::warn("no AUR index; run `gitaur -Sy` first");
        return Ok(1);
    }
    let idx = index::load(&path)?;
    let by = Secondary::build(&idx);

    let regexes: Vec<regex::Regex> = terms
        .iter()
        .map(|t| regex::RegexBuilder::new(t).case_insensitive(true).build())
        .collect::<std::result::Result<_, _>>()?;
    let mut hits = by.search(&idx, &regexes);
    // `search` is parallelised over the index so its return order is not
    // stable; sort by pkgbase so the picker shows the same rows in the same
    // positions across runs (and tests can pin a label order).
    hits.sort_by(|a, b| a.pkgbase.cmp(&b.pkgbase));
    info!(count = hits.len(), "search results");

    if hits.is_empty() {
        ui::info(&format!("no AUR packages match `{}`", terms.join(" ")));
        return Ok(0);
    }

    match pick(&hits, noconfirm)? {
        PickOutcome::Listed => Ok(0),
        PickOutcome::Aborted => Err(Error::UserAbort),
        PickOutcome::Picked(selected) => {
            debug!(picked = selected.len(), "search-install selection");
            let targets: Vec<Target> = selected.into_iter().map(Target::bare).collect();
            build::cmd_install(cfg, &targets, noconfirm, asdeps, false)
        }
    }
}

/// Render the picker (or, when non-interactive, dump labels to stdout and
/// install nothing — auto-installing every regex hit is too dangerous to do
/// without a human in the loop; the user can re-run interactively or with
/// `-S <pkg>` once they know the exact pkgname).
fn pick(hits: &[&IndexEntry], noconfirm: bool) -> Result<PickOutcome> {
    let labels_plain: Vec<String> = hits.iter().map(|e| label_plain(e)).collect();

    let interactive = !noconfirm && std::io::stdin().is_terminal();
    if !interactive {
        // Pipelines (`gitaur foo | grep …`) and `--noconfirm` callers both
        // land here. We print the matches so the search itself is useful and
        // exit cleanly so the shell doesn't treat the listing as a failure.
        for l in &labels_plain {
            println!("{l}");
        }
        return Ok(PickOutcome::Listed);
    }

    let labels_colored: Vec<String>;
    let labels_display: &[String] = if ui::color_on() {
        labels_colored = hits.iter().map(|e| label_colored(e)).collect();
        &labels_colored
    } else {
        &labels_plain
    };

    let chosen = MultiSelect::new()
        .with_prompt("Select packages to install (space toggles, enter confirms)")
        .items(labels_display)
        // Same rationale as the upgrade picker: dialoguer would otherwise
        // re-list every selected row as a single wrapped line that duplicates
        // the picker output. We print our own short summary instead.
        .report(false)
        .interact()
        .map_err(|e| Error::other(format!("search picker: {e}")))?;

    if chosen.is_empty() {
        return Ok(PickOutcome::Aborted);
    }
    Ok(PickOutcome::Picked(
        chosen
            .into_iter()
            .map(|i| hits[i].pkgbase.clone().into_inner())
            .collect(),
    ))
}

/// One picker row, plain ASCII — fed to dialoguer for width math.
fn label_plain(e: &IndexEntry) -> String {
    let ver = version_string(e);
    match e.pkgdesc.as_deref() {
        Some(d) if !d.is_empty() => format!("aur/{} {}  {}", e.pkgbase, ver, d),
        _ => format!("aur/{} {}", e.pkgbase, ver),
    }
}

/// Colored variant of [`label_plain`] — matches `-Ss` / install-table styling
/// (repo prefix dim, version green, description dimmed).
fn label_colored(e: &IndexEntry) -> String {
    let ver = version_string(e);
    let head = format!(
        "{} {}",
        style(format!("aur/{}", e.pkgbase)).bold(),
        style(ver).green(),
    );
    match e.pkgdesc.as_deref() {
        Some(d) if !d.is_empty() => format!("{head}  {}", ui::dim(d)),
        _ => head,
    }
}

fn version_string(e: &IndexEntry) -> String {
    match e.epoch.as_deref() {
        Some(ep) if !ep.is_empty() => format!("{ep}:{}-{}", e.pkgver, e.pkgrel),
        _ => format!("{}-{}", e.pkgver, e.pkgrel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::index::schema::Pkgname;

    fn mk(pkgbase: &str, desc: Option<&str>, epoch: Option<&str>) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
            }],
            pkgver: "1.2.3".into(),
            pkgrel: "4".into(),
            epoch: epoch.map(str::to_owned),
            pkgdesc: desc.map(str::to_owned),
            ..Default::default()
        }
    }

    /// `label_plain` is the byte-exact string dialoguer measures for wrap
    /// width — must stay free of ANSI escapes, must surface pkgbase / version
    /// / description so the user has enough to pick from.
    #[test]
    fn label_plain_no_ansi_and_has_all_pieces() {
        let l = label_plain(&mk("foo", Some("does foo"), None));
        assert!(!l.contains('\u{1b}'), "ANSI leaked into plain label: {l:?}");
        assert_eq!(l, "aur/foo 1.2.3-4  does foo");
    }

    #[test]
    fn label_plain_drops_empty_or_missing_description() {
        // No pkgdesc at all.
        assert_eq!(label_plain(&mk("bar", None, None)), "aur/bar 1.2.3-4");
        // Empty pkgdesc string (`pkgdesc=`) renders the same as None — no
        // trailing double-space dangler.
        assert_eq!(label_plain(&mk("baz", Some(""), None)), "aur/baz 1.2.3-4");
    }

    #[test]
    fn label_plain_includes_epoch_when_set() {
        let l = label_plain(&mk("qux", None, Some("2")));
        assert_eq!(l, "aur/qux 2:1.2.3-4");
    }

    #[test]
    fn label_plain_skips_empty_epoch_string() {
        // Mirrors `index::version_string`: `epoch = Some("")` (from `epoch=`
        // with no value) is treated as no epoch — must not render `:1.2.3-4`.
        let l = label_plain(&mk("qux", None, Some("")));
        assert!(l.starts_with("aur/qux 1.2.3-4"), "got: {l:?}");
    }
}
