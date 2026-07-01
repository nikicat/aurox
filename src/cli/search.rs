//! `gaur <term>...` — yay-style fuzzy search across the sync repos + the AUR.
//!
//! Wired up from [`crate::cli::dispatch`] for the no-operation-letter case.
//! Interactively, dispatch launches the shell REPL seeded with this search
//! (see [`crate::cli::shell`]) — there is no picker; the REPL is the one
//! interactive surface. This module owns the *non-interactive* path (a pipe or
//! `--noconfirm`): it merges sync-repo and AUR matches into one relevance-ranked
//! list (see [`rank_rows`]) and prints it, installing nothing. The [`Row`] model
//! and its labels are shared with the shell so both render matches identically.

use crate::config::Config;
use crate::error::Result;
use crate::index::{self, IndexEntry, secondary::Secondary};
use crate::names::{NameMatch, PkgTarget, SearchTerm};
use crate::pacman::alpm_db::{self, RepoHit};
use crate::pacman::invoke::REPO_AUR;
use crate::paths;
use crate::runopts;
use crate::ui;

use console::style;
use std::cmp::Ordering;
use tracing::{info, instrument};

/// One search hit — either a sync-repo package or an AUR pkgbase.
///
/// Borrows the AUR entry from the loaded index; repo hits are owned (their
/// `Alpm` handle is already dropped by the time we build rows). `pub(crate)` so
/// the interactive shell ([`crate::cli::shell`]) reuses the same row model +
/// labels for its numbered result list.
pub(crate) enum Row<'a> {
    Repo(RepoHit),
    Aur(&'a IndexEntry),
}

impl Row<'_> {
    /// The name to install if this row is picked, widened to the unclassified
    /// [`PkgTarget`] that the picker domain deals in (a repo pkgname or an AUR
    /// pkgbase — only the resolver tells them apart). Uses the existing
    /// `From<&PkgName>` / `From<&PkgBase>` widening conversions, so this is the
    /// only place the two row kinds collapse into one type, and there's no
    /// second dispatch downstream.
    pub(crate) fn picked(&self) -> PkgTarget {
        match self {
            Row::Repo(r) => PkgTarget::from(&r.name),
            Row::Aur(e) => PkgTarget::from(&e.pkgbase),
        }
    }

    /// The repo bucket this row belongs to (`core`, `extra`, …, or `aur`), for
    /// the shell's repo-filter selectors (`add extra`).
    pub(crate) fn repo_name(&self) -> &str {
        match self {
            Row::Repo(r) => r.repo.as_str(),
            Row::Aur(_) => REPO_AUR,
        }
    }

    /// The display label for this row (no leading number), colored per `color`.
    /// The shell prefixes it with the row number; the non-interactive listing
    /// prints the plain form as-is.
    pub(crate) fn label(&self, color: bool) -> String {
        if color {
            label_colored(self)
        } else {
            label_plain(self)
        }
    }
}

/// Entry point for the bare-positional shortcut in a **non-interactive** run
/// (a pipe, or `--noconfirm`).
///
/// The interactive case never reaches here — [`crate::cli::dispatch`] launches
/// the shell REPL seeded with the search instead, so there is no picker (the
/// REPL is the one interactive surface).
///
/// `terms` are the freeform regex fragments the user typed, combined as an AND
/// filter (same semantics as `-Ss`). Sync-repo and AUR matches are merged into
/// one relevance-ranked list ([`rank_rows`]) and printed best-first — so
/// `gaur foo | head` surfaces the strongest hits. Nothing is installed:
/// auto-installing every regex hit is too dangerous without a human in the loop.
#[instrument(skip(cfg))]
pub fn cmd_search_install(cfg: &Config, terms: &[SearchTerm]) -> Result<u8> {
    let regexes: Vec<regex::Regex> = terms
        .iter()
        .map(SearchTerm::compile)
        .collect::<std::result::Result<_, _>>()?;

    // Repo + AUR searches are independent I/O — an alpm DB scan vs an index
    // mmap. Run them concurrently and merge below.
    let (repo_res, aur_res) = rayon::join(
        || alpm_db::search_sync(terms),
        // `propagate` so `load_or_resync` sees `--noresync` even when rayon
        // runs this closure on a worker thread (its RunOpts TLS is otherwise
        // the default).
        runopts::propagate(|| -> Result<Option<index::IndexFile>> {
            let path = paths::index_path();
            if !path.exists() {
                return Ok(None);
            }
            Ok(Some(index::load_or_resync(cfg, &path)?))
        }),
    );
    let repo_hits = repo_res?;
    let idx = aur_res?;
    if idx.is_none() {
        ui::warn("no AUR index; showing repo matches only (run `gaur -Sy` to index the AUR)");
    }

    let aur_hits: Vec<&IndexEntry> = match idx.as_ref() {
        Some(idx) => {
            let by = Secondary::build(idx);
            by.search(idx, &regexes)
        }
        None => Vec::new(),
    };

    // Repo and AUR rows share one relevance-ranked list (unlike yay's fixed
    // "repos on top", `rank_rows` interleaves both sources by match quality).
    let mut rows: Vec<Row<'_>> = repo_hits
        .into_iter()
        .map(Row::Repo)
        .chain(aur_hits.into_iter().map(Row::Aur))
        .collect();
    rank_rows(&mut rows, &regexes);
    info!(rows = rows.len(), "search results");

    if rows.is_empty() {
        ui::info(&format!(
            "no packages match `{}`",
            terms
                .iter()
                .map(SearchTerm::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        ));
        return Ok(0);
    }

    // Plain labels (no ANSI) keep the listing pipe-clean; best-first order means
    // a `… | head` shows the strongest matches.
    for row in &rows {
        println!("{}", label_plain(row));
    }
    Ok(0)
}

/// Rank + sort merged repo/AUR search `rows` in place, best match first.
///
/// The order the shell list and the non-interactive listing both use:
///   1. **match tier** — a package-name *prefix* match beats a name *substring*
///      match beats a *description-only* match. (`regexes` is already applied as
///      the AND filter that produced `rows`, so every row matches *somewhere*;
///      the tier records *where*.)
///   2. **shorter name wins** within a tier — `claude` before `claude-desktop`.
///   3. repo rows sit ahead of AUR rows of otherwise-equal rank (pacman owns the
///      name), then AUR ties break **freshest-commit-first**, then name, for a
///      stable total order.
///
/// `pub(crate)` so [`crate::cli::shell`] ranks its combined list identically.
pub(crate) fn rank_rows(rows: &mut [Row<'_>], regexes: &[regex::Regex]) {
    rows.sort_by_cached_key(|r| rank_key(r, regexes));
}

/// The total-order sort key for one row — see [`rank_rows`] for the field
/// meanings. Field declaration order *is* the comparison order (derived `Ord`).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    tier: MatchTier,
    name_len: usize,
    source: SourceRank,
    /// Breaks AUR ties freshest-commit-first; repo rows all tie here (they've
    /// already been separated by `source`).
    freshness: Freshness,
    /// Final lexical tie-break — the row's install identity (`PkgTarget`).
    name: PkgTarget,
}

/// A row's freshness for ranking: its AUR branch-tip commit time, ordered so
/// **fresher sorts first** — a later commit is the better tie-break. Wrapping
/// the raw [`IndexEntry::commit_time_unix`] keeps that "fresher wins" polarity
/// in one place (an `impl Ord`) instead of scattering a bare `Reverse<i64>`
/// through the sort key.
#[derive(PartialEq, Eq)]
struct Freshness(i64);

impl Freshness {
    /// Rows with no commit of their own (repo packages) — older than any real
    /// AUR commit, so they never win a freshness tie-break.
    const STALE: Self = Self(i64::MIN);
}

impl Ord for Freshness {
    fn cmp(&self, other: &Self) -> Ordering {
        // Larger commit time = fresher = "less", so it lands first in the
        // best-first `RankKey` order.
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for Freshness {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Where the query matched a package's name, best to worst. Only the name
/// decides the tier; a hit that reached the row purely through its description
/// (or `provides`) lands in [`MatchTier::Desc`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    /// Some name starts with the query.
    Prefix,
    /// Some name contains the whole query, but none as a prefix.
    Substring,
    /// No single name carries the whole query — it matched the description.
    Desc,
}

/// Repo rows sort ahead of AUR rows when everything else ties.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SourceRank {
    Repo,
    Aur,
}

fn rank_key(row: &Row<'_>, regexes: &[regex::Regex]) -> RankKey {
    // `picked()` is the row's install identity (repo pkgname / AUR pkgbase) —
    // the same name the label shows, reused here for the length + lexical keys.
    let name = row.picked();
    let (source, freshness) = match row {
        Row::Repo(_) => (SourceRank::Repo, Freshness::STALE),
        Row::Aur(e) => (SourceRank::Aur, Freshness(e.commit_time_unix)),
    };
    RankKey {
        tier: match_tier(row, regexes),
        name_len: name.len(),
        source,
        freshness,
        name,
    }
}

/// The best (lowest) tier any of a row's names achieves against the whole query.
///
/// A row's names are its display name plus — for AUR split packages — each
/// member pkgname, so a query hitting only a member still counts as a name
/// match, not a description one. Each name is tiered through its typed
/// `regex_anchor` (on `PkgName` / `PkgBase`); `name_tier` combines the per-term
/// anchors into a [`MatchTier`].
fn match_tier(row: &Row<'_>, regexes: &[regex::Regex]) -> MatchTier {
    match row {
        Row::Repo(r) => name_tier(|re| r.name.regex_anchor(re), regexes),
        Row::Aur(e) => e
            .pkgnames
            .iter()
            .map(|p| name_tier(|re| p.name.regex_anchor(re), regexes))
            .fold(
                name_tier(|re| e.pkgbase.regex_anchor(re), regexes),
                MatchTier::min,
            ),
    }
}

/// Tier one name against the whole query, given `anchor` — where each term
/// matches that name. The query is an AND, so the name has to satisfy *every*
/// term (`anchor` returning `Some`) to count as a name match at all: it's
/// `Prefix` when some term anchors at the name's start, `Substring` when all
/// terms match but none anchors, else `Desc` (the row was pulled in by its
/// description). The typed [`NameMatch`] keeps an anchored query like `^foo$`
/// classified as the exact-name match it is.
fn name_tier(
    anchor: impl Fn(&regex::Regex) -> Option<NameMatch>,
    regexes: &[regex::Regex],
) -> MatchTier {
    let mut any_prefix = false;
    for r in regexes {
        match anchor(r) {
            Some(NameMatch::Prefix) => any_prefix = true,
            Some(NameMatch::Inside) => {}
            None => return MatchTier::Desc,
        }
    }
    if any_prefix {
        MatchTier::Prefix
    } else {
        MatchTier::Substring
    }
}

/// One result row, plain ASCII (no ANSI) — the pipe-clean listing form and the
/// base the shell prefixes its row number onto.
fn label_plain(row: &Row<'_>) -> String {
    match row {
        Row::Repo(r) => {
            let installed = if r.installed { " [installed]" } else { "" };
            match r.desc.as_deref() {
                Some(d) if !d.is_empty() => {
                    format!("{}/{} {}{installed}  {d}", r.repo, r.name, r.version)
                }
                _ => format!("{}/{} {}{installed}", r.repo, r.name, r.version),
            }
        }
        Row::Aur(e) => {
            let ver = aur_version(e);
            match e.display_desc() {
                Some(d) => format!("aur/{} {ver}  {d}", e.pkgbase),
                None => format!("aur/{} {ver}", e.pkgbase),
            }
        }
    }
}

/// Colored variant of [`label_plain`] — matches `-Ss` / install-table styling
/// (repo prefix bold, version green, description dimmed, installed marker cyan).
fn label_colored(row: &Row<'_>) -> String {
    match row {
        Row::Repo(r) => {
            let mut head = format!(
                "{}/{} {}",
                ui::repo(r.repo.as_str()),
                style(&r.name).bold(),
                style(&r.version).green(),
            );
            if r.installed {
                head = format!("{head} {}", style("[installed]").cyan());
            }
            match r.desc.as_deref() {
                Some(d) if !d.is_empty() => format!("{head}  {}", ui::dim(d)),
                _ => head,
            }
        }
        Row::Aur(e) => {
            let ver = aur_version(e);
            let head = format!(
                "{}/{} {}",
                ui::repo("aur"),
                style(&e.pkgbase).bold(),
                style(ver).green(),
            );
            match e.display_desc() {
                Some(d) => format!("{head}  {}", ui::dim(d)),
                None => head,
            }
        }
    }
}

fn aur_version(e: &IndexEntry) -> String {
    match e.epoch.as_deref() {
        Some(ep) if !ep.is_empty() => format!("{ep}:{}-{}", e.pkgver, e.pkgrel),
        _ => format!("{}-{}", e.pkgver, e.pkgrel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::index::schema::Pkgname;
    use crate::names::PkgName;
    use crate::version::Version;

    fn mk(pkgbase: &str, desc: Option<&str>, epoch: Option<&str>) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            pkgver: "1.2.3".into(),
            pkgrel: "4".into(),
            epoch: epoch.map(str::to_owned),
            pkgdesc: desc.map(str::to_owned),
            ..Default::default()
        }
    }

    fn repo(name: &str, desc: Option<&str>, installed: bool) -> RepoHit {
        RepoHit {
            repo: "extra".into(),
            name: PkgName::new(name),
            version: Version::from("2.0-1"),
            desc: desc.map(str::to_owned),
            installed,
        }
    }

    /// `label_plain` is the pipe-clean listing form — must stay free of ANSI
    /// escapes and surface pkgbase / version / description so the user has
    /// enough to act on.
    #[test]
    fn aur_label_plain_no_ansi_and_has_all_pieces() {
        let l = label_plain(&Row::Aur(&mk("foo", Some("does foo"), None)));
        assert!(!l.contains('\u{1b}'), "ANSI leaked into plain label: {l:?}");
        assert_eq!(l, "aur/foo 1.2.3-4  does foo");
    }

    #[test]
    fn aur_label_plain_drops_empty_or_missing_description() {
        assert_eq!(
            label_plain(&Row::Aur(&mk("bar", None, None))),
            "aur/bar 1.2.3-4"
        );
        assert_eq!(
            label_plain(&Row::Aur(&mk("baz", Some(""), None))),
            "aur/baz 1.2.3-4"
        );
    }

    #[test]
    fn aur_label_plain_includes_epoch_when_set() {
        let l = label_plain(&Row::Aur(&mk("qux", None, Some("2"))));
        assert_eq!(l, "aur/qux 2:1.2.3-4");
    }

    #[test]
    fn aur_label_plain_skips_empty_epoch_string() {
        let l = label_plain(&Row::Aur(&mk("qux", None, Some(""))));
        assert!(l.starts_with("aur/qux 1.2.3-4"), "got: {l:?}");
    }

    /// Repo rows render in pacman `repo/name version` shape, with the
    /// `[installed]` marker only when the user already has the package.
    #[test]
    fn repo_label_plain_shape_and_installed_marker() {
        assert_eq!(
            label_plain(&Row::Repo(repo("firefox", Some("a browser"), false))),
            "extra/firefox 2.0-1  a browser"
        );
        assert_eq!(
            label_plain(&Row::Repo(repo("vim", None, true))),
            "extra/vim 2.0-1 [installed]"
        );
    }

    /// Both row kinds widen to the unclassified `PkgTarget` the install path
    /// consumes — repo rows from their pkgname, AUR rows from their pkgbase.
    /// The resolver (not the picker) re-classifies, so the picker only has to
    /// hand over the name string in the right type.
    #[test]
    fn picked_widens_repo_pkgname_and_aur_pkgbase() {
        assert_eq!(
            Row::Repo(repo("firefox", None, false)).picked(),
            PkgTarget::from("firefox")
        );
        let e = mk("bisq", None, None);
        assert_eq!(Row::Aur(&e).picked(), PkgTarget::from("bisq"));
    }

    /// Compile domain search terms into the regexes ranking consumes.
    fn compiled(terms: &[SearchTerm]) -> Vec<regex::Regex> {
        terms.iter().map(|t| t.compile().unwrap()).collect()
    }

    /// Rank `rows` against `terms` and return the install identities in order.
    fn ranked(mut rows: Vec<Row<'_>>, terms: &[SearchTerm]) -> Vec<PkgTarget> {
        rank_rows(&mut rows, &compiled(terms));
        rows.iter().map(Row::picked).collect()
    }

    /// The primary key: a name-prefix hit outranks a name-substring hit, which
    /// outranks a description-only hit.
    #[test]
    fn rank_orders_prefix_then_substring_then_desc() {
        let substr = mk("py-claude", None, None); // "claude" at index 3
        let prefix = mk("claude", None, None);
        let desc = mk("toolkit", Some("wraps claude"), None); // name lacks the term
        let rows = vec![Row::Aur(&substr), Row::Aur(&desc), Row::Aur(&prefix)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("claude")]),
            [
                PkgTarget::from("claude"),
                PkgTarget::from("py-claude"),
                PkgTarget::from("toolkit"),
            ]
        );
    }

    /// Within a tier, the shorter name wins.
    #[test]
    fn rank_prefers_shorter_name_within_tier() {
        let long = mk("claude-desktop", None, None);
        let short = mk("claude", None, None);
        let rows = vec![Row::Aur(&long), Row::Aur(&short)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("claude")]),
            [PkgTarget::from("claude"), PkgTarget::from("claude-desktop")]
        );
    }

    /// Equal tier + equal name length: a repo row sorts ahead of an AUR one.
    #[test]
    fn rank_puts_repo_ahead_of_aur_on_equal_match() {
        let aur = mk("claude", None, None);
        let mut rows = vec![Row::Aur(&aur), Row::Repo(repo("claude", None, false))];
        rank_rows(&mut rows, &compiled(&[SearchTerm::new("claude")]));
        assert!(matches!(rows[0], Row::Repo(_)), "repo should lead the tie");
        assert!(matches!(rows[1], Row::Aur(_)));
    }

    /// `Freshness` is the domain key behind the AUR tie-break: a newer commit
    /// sorts *before* an older one, and repo rows' `STALE` sorts last.
    #[test]
    fn freshness_orders_newer_before_older() {
        assert!(Freshness(900) < Freshness(100));
        assert!(Freshness(100) < Freshness::STALE);
    }

    /// End to end, that tie-break beats the lexical fallback (`aaa-` would
    /// otherwise precede `zzz-`): the fresher pkgbase leads.
    #[test]
    fn rank_breaks_aur_ties_by_freshest_commit() {
        let mut old = mk("aaa-claude", None, None);
        old.commit_time_unix = 100;
        let mut fresh = mk("zzz-claude", None, None);
        fresh.commit_time_unix = 900;
        let rows = vec![Row::Aur(&old), Row::Aur(&fresh)];
        assert_eq!(
            ranked(rows, &[SearchTerm::new("claude")]),
            [PkgTarget::from("zzz-claude"), PkgTarget::from("aaa-claude")]
        );
    }

    /// An anchored regex (`^name$`) still classifies as an exact name-prefix
    /// match — the tier is computed from the compiled regex, not raw text.
    #[test]
    fn rank_treats_anchored_regex_as_name_prefix() {
        let hit = mk("test-trivial", None, None);
        let miss = mk("unrelated", None, None);
        let rx = compiled(&[SearchTerm::new("^test-trivial$")]);
        assert_eq!(match_tier(&Row::Aur(&hit), &rx), MatchTier::Prefix);
        assert_eq!(match_tier(&Row::Aur(&miss), &rx), MatchTier::Desc);
    }

    /// Multi-term AND: a name-tier match needs *every* term in the name. Here
    /// `python-claude` carries both (→ prefix), while `claude-cli` has "python"
    /// only in its description (→ desc), so it ranks lower.
    #[test]
    fn rank_multi_term_requires_all_terms_in_name() {
        let both = mk("python-claude", None, None);
        let one = mk("claude-cli", Some("a python helper"), None);
        let rows = vec![Row::Aur(&one), Row::Aur(&both)];
        assert_eq!(
            ranked(
                rows,
                &[SearchTerm::new("python"), SearchTerm::new("claude")]
            ),
            [
                PkgTarget::from("python-claude"),
                PkgTarget::from("claude-cli")
            ]
        );
    }

    /// A split package's member pkgname counts as a name match, not a
    /// description one — so a query hitting only a member still ranks by name.
    #[test]
    fn rank_member_pkgname_counts_as_name_match() {
        let mut e = mk("widgets", None, None);
        e.pkgnames.push(Pkgname {
            name: PkgName::new("libclaude"),
            provides: Vec::new(),
            pkgdesc: None,
        });
        let rx = compiled(&[SearchTerm::new("claude")]);
        assert_eq!(match_tier(&Row::Aur(&e), &rx), MatchTier::Substring);
    }
}
