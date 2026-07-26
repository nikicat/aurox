//! Package info blocks, merged across the sync repos and the AUR.
//!
//! [`InfoLookup`] is the one engine behind both `-Si` ([`cmd_info`]) and the
//! shell's `info`. The repo block is rendered by
//! [`SyncInfo`](crate::pacman::alpm_db::SyncInfo); the AUR block lives here,
//! with most fields straight from the [`IndexEntry`] and three resolved live
//! per target:
//!
//! * **Maintainer** — the `# Maintainer:` comment convention in the PKGBUILD
//!   blob at the entry's indexed tip commit (the AUR RPC's Maintainer field
//!   has no git-side equivalent, but the comment carries the same
//!   information).
//! * **First Submitted** — the committer time of the branch's root commit.
//! * **Installed Size** — localdb `isize()` for members already installed
//!   (an AUR pkgbase has no syncdb to quote a size from before it's built).

use crate::cli::Outcome;
use crate::config::Config;
use crate::error::Result;
use crate::index::schema::IndexEntry;
use crate::index::{AurIndexData, AurState};
use crate::names::{Maintainer, PkgName, PkgTarget};
use crate::pacman::alpm_db;
use crate::paths;
use crate::ui::{self, Paint};
use crate::units::{ByteSize, UnixTime};
use alpm::Alpm;
use console::{StyledObject, style};
use gix::ObjectId;
use std::fmt::Display;
use std::io::{self, Write};
use tracing::debug;

/// `-Si` info for one or more targets — one [`InfoLookup`] pass.
///
/// The AUR side loads *empty* when not in play, so the lookup is uniform;
/// only the final "nothing found" wording consults [`AurState`].
/// [`Outcome::NotFound`] when any requested target was nowhere to be found.
pub fn cmd_info(cfg: &Config, targets: &[PkgTarget]) -> Result<Outcome> {
    let data = AurIndexData::load(cfg)?;
    let missing = InfoLookup::open(&data)?.print_all(targets);
    if missing.is_empty() {
        return Ok(Outcome::Done);
    }
    ui::warn(&missing_warning(
        AurState::probe(cfg),
        &missing,
        "`aurox -Sy`",
    ));
    Ok(Outcome::NotFound)
}

/// Word the "nothing found" warning honestly: only claim the AUR was
/// consulted when its data was actually in play. `sync_hint` is the calling
/// surface's way to sync the AUR (the CLI's `` `aurox -Sy` ``, the shell's
/// `` `refresh` ``).
pub(crate) fn missing_warning(aur: AurState, missing: &[&PkgTarget], sync_hint: &str) -> String {
    let names = missing
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match aur {
        AurState::Ready => format!("not in repos or AUR: {names}"),
        AurState::NotSetUp => {
            format!("not in repos: {names} (no AUR index — {sync_hint} enables AUR lookups)")
        }
        AurState::Disabled => {
            format!("not in repos: {names} (AUR disabled: aur = false in config.toml)")
        }
    }
}

/// One merged repo + AUR info query: the loaded AUR data plus the live
/// handles the blocks draw on. The one engine behind both `-Si`
/// ([`cmd_info`]) and the shell's `info`, so the two surfaces can't drift.
///
/// The sync DBs are required — every lookup consults them first. The bare
/// AUR mirror (behind the AUR block's maintainer / first-submitted fields)
/// is best-effort: a mirror that fails to open just leaves those fields off
/// every block, never fails the command.
pub(crate) struct InfoLookup<'a> {
    data: &'a AurIndexData,
    alpm: Alpm,
    mirror: Option<gix::Repository>,
    /// Resolved once per lookup, not per block: every block this pass prints
    /// goes to the same terminal.
    paint: Paint,
}

impl<'a> InfoLookup<'a> {
    /// Open the live handles next to the loaded AUR `data`.
    pub(crate) fn open(data: &'a AurIndexData) -> Result<Self> {
        let mirror = gix::open(paths::aur_repo_path())
            .inspect_err(|e| debug!(error = %e, "mirror unavailable for info extras"))
            .ok();
        Ok(Self {
            data,
            alpm: alpm_db::open()?,
            mirror,
            paint: Paint::detect(),
        })
    }

    /// Print the info block for each target in order (so multi-target output
    /// matches pacman's) and return the targets found in neither source.
    ///
    /// Repo wins ties: pacman owns a name that lives in both a sync repo and
    /// the AUR (`info cef` must describe extra/cef, not the same-named AUR
    /// pkgbase) — the same rule as `classify` and the resolver.
    pub(crate) fn print_all<'t>(&self, targets: &'t [PkgTarget]) -> Vec<&'t PkgTarget> {
        targets.iter().filter(|t| !self.print_one(t)).collect()
    }

    /// One target: the sync repos first, then the AUR (pkgname / provides /
    /// pkgbase, via [`AurIndexData::entry`]). `false` ⇒ found in neither —
    /// the caller words the miss.
    fn print_one(&self, target: &PkgTarget) -> bool {
        if let Some(info) = alpm_db::SyncInfo::lookup(&self.alpm, target.bare()) {
            info.print(self.paint);
            return true;
        }
        match self.data.entry(target) {
            Some(entry) => {
                print_info(entry, &self.extras(entry), self.paint);
                true
            }
            None => false,
        }
    }

    /// Resolve the out-of-index extras for one AUR entry. Each lookup is
    /// independently best-effort: a branch whose history can't be walked
    /// still gets its installed size, and vice versa.
    fn extras(&self, e: &IndexEntry) -> Extras {
        let mut x = Extras::default();
        let tip = ObjectId::from(e.commit_oid);
        if let Some(repo) = &self.mirror
            && !tip.is_null()
        {
            x.maintainers = maintainers_at(repo, tip).unwrap_or_default();
            x.first_submitted = first_submitted(repo, tip);
        }
        for p in &e.pkgnames {
            if let Ok(pkg) = self.alpm.localdb().pkg(p.name.as_str()) {
                x.installed.push(InstalledMember {
                    name: p.name.clone(),
                    size: ByteSize::new(u64::try_from(pkg.isize()).unwrap_or(0)),
                });
            }
        }
        x
    }
}

/// The out-of-index half of one entry's block (see [`InfoLookup::extras`]).
/// Absent fields simply omit their lines — [`Extras::default`] renders the
/// same block the index alone can produce.
#[derive(Default)]
struct Extras {
    maintainers: Vec<Maintainer>,
    first_submitted: Option<UnixTime>,
    /// Already-installed members, in `pkgnames` order.
    installed: Vec<InstalledMember>,
}

/// One already-installed member of the pkgbase and its localdb `isize`.
struct InstalledMember {
    name: PkgName,
    size: ByteSize,
}

/// Print the block to stdout (the interactive path). Same best-effort stance
/// as the `println!`-based printers elsewhere: a closed stdout mid-block
/// isn't worth failing the command over.
fn print_info(e: &IndexEntry, x: &Extras, paint: Paint) {
    let stdout = io::stdout();
    write_info(stdout.lock(), e, x, paint).ok();
}

/// Render the block to `out` in pacman's `-Si` field order (aurox-specific
/// fields slot in next to their nearest pacman analogue). Empty fields are
/// omitted, not rendered as `None` — the long-standing aurox stance. A
/// writer (not `println!`) so the exact byte layout is testable without
/// capturing a process's stdout.
fn write_info<W: Write>(out: W, e: &IndexEntry, x: &Extras, paint: Paint) -> io::Result<()> {
    let mut b = InfoBlock::new(out, paint);
    b.accent(Label::Repository, "aur", repo_accent)?;
    b.field(Label::Name, &e.pkgbase)?;
    // Show the split-pkg list whenever the entry actually has more than one
    // pkgname (or the single pkgname differs from pkgbase). Members carrying
    // their own pkgdesc render as `name: desc`, so split-package descriptions
    // surface without a per-member block.
    let trivial = e.pkgnames.len() == 1 && e.pkgbase.matches_pkgname(&e.pkgnames[0].name);
    if !e.pkgnames.is_empty() && !trivial {
        let members: Vec<String> = e
            .pkgnames
            .iter()
            .map(|p| match p.pkgdesc.as_deref() {
                Some(d) if !d.is_empty() => format!("{}: {d}", p.name),
                _ => p.name.to_string(),
            })
            .collect();
        b.multiline(Label::SplitPkgs, &members)?;
    }
    b.accent(Label::Version, e.version().as_str(), version_accent)?;
    if let Some(d) = e.display_desc() {
        b.field(Label::Description, d)?;
    }
    b.list(Label::Architecture, &e.arch)?;
    if let Some(u) = &e.url {
        b.accent(Label::Url, u.as_str(), url_accent)?;
    }
    // Union of pkgbase-level and pkgname-scoped provides — `-Si` users
    // want to see every virtual name the pkgbase makes available, not the
    // attribution.
    let provides: Vec<&PkgTarget> = e.all_provides().collect();
    b.list(Label::Provides, &provides)?;
    b.list(Label::DependsOn, &e.depends)?;
    b.list(Label::MakeDeps, &e.makedepends)?;
    b.list(Label::CheckDeps, &e.checkdepends)?;
    // One optdep per line, pacman-style — the `: reason` halves would blur
    // together space-joined.
    let optdeps: Vec<String> = e.optdepends.iter().map(ToString::to_string).collect();
    b.multiline(Label::OptionalDeps, &optdeps)?;
    b.list(Label::ConflictsWith, &e.conflicts)?;
    b.list(Label::Replaces, &e.replaces)?;
    // localdb sizes exist only for installed members. Split packages label
    // each member's line; the trivial single-pkgname case is just the size.
    let sizes: Vec<String> = x
        .installed
        .iter()
        .map(|m| {
            if trivial {
                m.size.to_string()
            } else {
                format!("{}: {}", m.name, m.size)
            }
        })
        .collect();
    b.multiline(Label::InstalledSize, &sizes)?;
    let maintainers: Vec<String> = x.maintainers.iter().map(ToString::to_string).collect();
    b.multiline(Label::Maintainer, &maintainers)?;
    if let Some(t) = x.first_submitted.and_then(UnixTime::render) {
        b.field(Label::FirstSubmitted, t)?;
    }
    if let Some(t) = e.commit_time.render() {
        b.field(Label::LastUpdated, t)?;
    }
    b.end()
}

/// A field label of the info block — the closed vocabulary both blocks
/// (AUR here, repo in [`crate::pacman::alpm_db::SyncInfo`]) draw from, so
/// they can't drift into near-miss labels ("Depends" vs "Depends On") and
/// a typo'd free string can't compile. Every label must fit the 16-column
/// gutter [`field`] aligns on; a unit test pins that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Label {
    Repository,
    Name,
    SplitPkgs,
    Version,
    Description,
    Architecture,
    Url,
    Provides,
    DependsOn,
    MakeDeps,
    CheckDeps,
    OptionalDeps,
    ConflictsWith,
    Replaces,
    DownloadSize,
    InstalledSize,
    Maintainer,
    Packager,
    FirstSubmitted,
    LastUpdated,
    BuildDate,
}

impl Label {
    /// Every variant, for the gutter-width test.
    #[cfg(test)]
    const ALL: [Self; 21] = [
        Self::Repository,
        Self::Name,
        Self::SplitPkgs,
        Self::Version,
        Self::Description,
        Self::Architecture,
        Self::Url,
        Self::Provides,
        Self::DependsOn,
        Self::MakeDeps,
        Self::CheckDeps,
        Self::OptionalDeps,
        Self::ConflictsWith,
        Self::Replaces,
        Self::DownloadSize,
        Self::InstalledSize,
        Self::Maintainer,
        Self::Packager,
        Self::FirstSubmitted,
        Self::LastUpdated,
        Self::BuildDate,
    ];

    const fn text(self) -> &'static str {
        match self {
            Self::Repository => "Repository",
            Self::Name => "Name",
            Self::SplitPkgs => "Split pkgs",
            Self::Version => "Version",
            Self::Description => "Description",
            Self::Architecture => "Architecture",
            Self::Url => "URL",
            Self::Provides => "Provides",
            Self::DependsOn => "Depends On",
            Self::MakeDeps => "Make Deps",
            Self::CheckDeps => "Check Deps",
            Self::OptionalDeps => "Optional Deps",
            Self::ConflictsWith => "Conflicts With",
            Self::Replaces => "Replaces",
            Self::DownloadSize => "Download Size",
            Self::InstalledSize => "Installed Size",
            Self::Maintainer => "Maintainer",
            Self::Packager => "Packager",
            Self::FirstSubmitted => "First Submitted",
            Self::LastUpdated => "Last Updated",
            Self::BuildDate => "Build Date",
        }
    }
}

impl Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.text())
    }
}

/// One info block being written: the sink plus the paint it renders under.
///
/// Both blocks — the AUR one here and the repo one in
/// [`SyncInfo`](crate::pacman::alpm_db::SyncInfo) — write through this, so the
/// [`GUTTER`], the label styling, and the value accents are decided once
/// and the two can't drift apart. Naming the pair also keeps `paint` off every
/// one of the twenty-odd field calls.
///
/// Color is **additive and identity-first**, the same doctrine the search list
/// renders under: the label column carries pacman's bold title styling, the
/// three cells that say *which package this is* (repo, version, URL) get an
/// accent, and everything else stays plain so the block reads as text, not as
/// a light show. The repo accent is the shared hashed color from
/// [`ui::repo`](crate::ui::repo), so `extra` looks the same here as in the
/// search list and the transaction table.
pub(crate) struct InfoBlock<W: Write> {
    out: W,
    paint: Paint,
}

/// The label gutter: every value starts at this column — pacman's `-Si`
/// layout, a fixed width rather than one measured over the labels present
/// (which would move the colon from block to block). Every [`Label`] must fit
/// it; a test pins that.
///
/// This is why the block doesn't render through [`ui::Grid`](crate::ui::Grid):
/// the grid separates columns with a fixed two blanks, so it can't place a
/// `": "` at a fixed column, and its whole job — measuring columns over rows,
/// padding colored cells by visible width — has nothing to do here, where the
/// one padded cell is a plain-ASCII label of constant width.
const GUTTER: usize = 16;

/// Where a multiline field's continuation lines start: past the gutter and
/// the `": "` that follows it. Derived, so the two can't drift.
const CONTINUATION: usize = GUTTER + ": ".len();

impl<W: Write> InfoBlock<W> {
    pub(crate) const fn new(out: W, paint: Paint) -> Self {
        Self { out, paint }
    }

    /// One field line: the label padded to [`GUTTER`] + `: ` + value —
    /// pacman's `-Si` alignment.
    pub(crate) fn field(&mut self, label: Label, value: impl Display) -> io::Result<()> {
        // Pad *before* styling: ANSI escapes are zero-width but not
        // zero-length, so styling first would make the pad measure the codes
        // and knock the gutter out of alignment.
        let padded = format!("{label:<GUTTER$}");
        if self.paint.colored() {
            writeln!(self.out, "{}: {value}", style(padded).bold())
        } else {
            writeln!(self.out, "{padded}: {value}")
        }
    }

    /// A field whose value carries an accent when color is on. Mirrors
    /// [`ui::Cell::paint`](crate::ui::Cell) — the styling closure runs only
    /// under [`Paint::Colored`], and the plain form is the bare value.
    pub(crate) fn accent(
        &mut self,
        label: Label,
        value: &str,
        style: impl FnOnce(&str) -> StyledObject<String>,
    ) -> io::Result<()> {
        if self.paint.colored() {
            self.field(label, style(value))
        } else {
            self.field(label, value)
        }
    }

    /// Space-joined list field, omitted when empty.
    pub(crate) fn list(&mut self, label: Label, items: &[impl Display]) -> io::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let joined = items
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        self.field(label, joined)
    }

    /// One value per line, continuation lines indented to the value column —
    /// how pacman renders `Optional Deps`. Omitted when empty.
    pub(crate) fn multiline(&mut self, label: Label, values: &[String]) -> io::Result<()> {
        for (i, v) in values.iter().enumerate() {
            if i == 0 {
                self.field(label, v)?;
            } else {
                writeln!(self.out, "{:<CONTINUATION$}{v}", "")?;
            }
        }
        Ok(())
    }

    /// The blank line that closes a block.
    pub(crate) fn end(&mut self) -> io::Result<()> {
        writeln!(self.out)
    }
}

/// The accent for a repo name — the shared hashed color, so `extra` reads the
/// same in an info block as in the search list and the transaction table. A
/// concrete `fn(&str)` wrapper: [`ui::repo`] is generic over `AsRef<str>`, so
/// it can't be handed to [`InfoBlock::accent`] directly.
pub(crate) fn repo_accent(r: &str) -> StyledObject<String> {
    ui::repo(r)
}

/// The accent for a version cell — green, matching the install table's "this
/// is the version you'd get".
pub(crate) fn version_accent(v: &str) -> StyledObject<String> {
    style(v.to_owned()).green()
}

/// The accent for a URL — cyan, the one field a reader scans for rather than
/// reads.
pub(crate) fn url_accent(u: &str) -> StyledObject<String> {
    style(u.to_owned()).cyan()
}

/// `# Maintainer:` comment values from a PKGBUILD, in file order.
///
/// An AUR convention, not machine-enforced: the current maintainer(s) head
/// the file as `# Maintainer: Name <email>`, previous ones demoted to
/// `# Contributor:`. Whole-file scan since nothing guarantees the header
/// block comes first; only comment lines are considered.
fn maintainers(pkgbuild: &str) -> Vec<Maintainer> {
    pkgbuild
        .lines()
        .filter_map(|line| {
            let comment = line.trim_start().strip_prefix('#')?;
            let (key, value) = comment.split_once(':')?;
            let key = key.trim();
            (key.eq_ignore_ascii_case("maintainer") || key.eq_ignore_ascii_case("maintainers"))
                .then(|| value.trim())
                .filter(|v| !v.is_empty())
                .map(Maintainer::new)
        })
        .collect()
}

/// [`maintainers`] over the PKGBUILD blob at the entry's indexed tip commit.
/// `None` on any lookup failure — the block just omits the field.
fn maintainers_at(repo: &gix::Repository, tip: ObjectId) -> Option<Vec<Maintainer>> {
    let tree = repo.find_commit(tip).ok()?.tree().ok()?;
    let entry = tree.find_entry("PKGBUILD")?;
    let blob = repo.find_object(entry.oid().to_owned()).ok()?;
    Some(maintainers(&String::from_utf8_lossy(blob.data.as_slice())))
}

/// Committer time of the branch's root commit — when the pkgbase first
/// appeared on the AUR. Walks the whole branch history; AUR package
/// histories are short (typically tens of commits), so per-target cost is
/// negligible. Multiple roots (a history graft) take the earliest. `None`
/// on any walk hiccup — the block just omits the field.
fn first_submitted(repo: &gix::Repository, tip: ObjectId) -> Option<UnixTime> {
    let walk = repo.find_commit(tip).ok()?.ancestors().all().ok()?;
    let mut oldest: Option<i64> = None;
    for info in walk {
        let info = info.ok()?;
        if info.parent_ids.is_empty() {
            let t = info.object().ok()?.time().ok()?.seconds;
            oldest = Some(oldest.map_or(t, |o: i64| o.min(t)));
        }
    }
    oldest.map(UnixTime::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::Pkgname;
    use crate::names::{Arch, Url};
    use crate::{assert_contains, assert_not_contains};

    /// Plain paint, always: the block's byte layout is what these pin, and
    /// `cargo test` under `makepkg`'s `check()` runs on a tty.
    fn render(e: &IndexEntry, x: &Extras) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_info(&mut buf, e, x, Paint::Plain).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn mk(pkgbase: &str) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            pkgver: "1.0".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    fn member(name: &str, desc: Option<&str>) -> Pkgname {
        Pkgname {
            name: name.into(),
            provides: Vec::new(),
            pkgdesc: desc.map(str::to_owned),
        }
    }

    /// Color is styling only: strip the ANSI from a colored block and it must
    /// be byte-identical to the plain one. Guards the padding rule (pad the
    /// label to the gutter *first*, style it after — the other order measures
    /// the escape codes and shifts every value column) and any field the
    /// colored path might drop. Real on a tty, where `console` actually emits
    /// codes — `makepkg`'s `check()` runs there.
    #[test]
    fn colored_block_strips_back_to_the_plain_one() {
        let mut e = mk("foo");
        e.pkgdesc = Some("does foo".into());
        e.url = Some(Url::new("https://foo.example"));
        e.depends = vec![PkgTarget::new("glibc>=2.38")];
        e.optdepends = vec!["cups: printing support".into(), "bash-completion".into()];
        let x = Extras {
            maintainers: vec![Maintainer::new("Jane Doe <jane@example.org>")],
            ..Extras::default()
        };
        let mut buf: Vec<u8> = Vec::new();
        write_info(&mut buf, &e, &x, Paint::Colored).unwrap();
        let colored = String::from_utf8(buf).unwrap();
        assert_eq!(console::strip_ansi_codes(&colored), render(&e, &x));
    }

    #[test]
    fn minimal_entry_renders_header_fields_only() {
        let out = render(&mk("foo"), &Extras::default());
        assert_eq!(
            out,
            "Repository      : aur\n\
             Name            : foo\n\
             Version         : 1.0-1\n\
             \n"
        );
    }

    #[test]
    fn full_entry_renders_pacman_si_field_order() {
        let mut e = mk("foo");
        e.pkgdesc = Some("does foo".into());
        e.url = Some(Url::new("https://foo.example"));
        e.arch = vec![Arch::new("i686"), Arch::new("x86_64")];
        e.provides = vec![PkgTarget::new("libfoo.so")];
        e.depends = vec![PkgTarget::new("glibc>=2.38")];
        e.makedepends = vec![PkgTarget::new("cmake")];
        e.checkdepends = vec![PkgTarget::new("python-pytest")];
        e.optdepends = vec!["cups: printing support".into(), "bash-completion".into()];
        e.conflicts = vec![PkgTarget::new("foo-git")];
        e.replaces = vec![PkgTarget::new("foo-legacy")];
        let out = render(&e, &Extras::default());
        assert_eq!(
            out,
            "Repository      : aur\n\
             Name            : foo\n\
             Version         : 1.0-1\n\
             Description     : does foo\n\
             Architecture    : i686 x86_64\n\
             URL             : https://foo.example\n\
             Provides        : libfoo.so\n\
             Depends On      : glibc>=2.38\n\
             Make Deps       : cmake\n\
             Check Deps      : python-pytest\n\
             Optional Deps   : cups: printing support\n                  bash-completion\n\
             Conflicts With  : foo-git\n\
             Replaces        : foo-legacy\n\
             \n"
        );
    }

    #[test]
    fn extras_render_after_the_index_fields() {
        let mut e = mk("foo");
        e.commit_time = UnixTime::new(1_700_000_000);
        let x = Extras {
            maintainers: vec![Maintainer::new("Jane Doe <jane@example.org>")],
            first_submitted: Some(UnixTime::new(1_600_000_000)),
            installed: vec![InstalledMember {
                name: PkgName::new("foo"),
                size: ByteSize::new(12 * 1024 * 1024),
            }],
        };
        let out = render(&e, &x);
        assert_contains!(out, "Installed Size  : 12.00 MiB\n");
        assert_contains!(out, "Maintainer      : Jane Doe <jane@example.org>\n");
        // System-timezone rendering makes the exact text environment-dependent;
        // presence and ordering are what this pins.
        let submitted = out.find("First Submitted").unwrap();
        let updated = out.find("Last Updated").unwrap();
        assert!(submitted < updated, "field order regressed:\n{out}");
    }

    #[test]
    fn unknown_commit_time_omits_last_updated() {
        // The `UnixTime` sentinel (entries from pre-v4 archives).
        let out = render(&mk("foo"), &Extras::default());
        assert_not_contains!(out, "Last Updated");
    }

    #[test]
    fn split_members_render_one_per_line_with_their_desc() {
        let mut e = mk("bisq");
        e.pkgnames = vec![
            member("bisq-desktop", Some("Desktop client")),
            member("bisq-cli", None),
        ];
        let out = render(&e, &Extras::default());
        assert_contains!(out, "Split pkgs      : bisq-desktop: Desktop client\n");
        assert_contains!(out, "                  bisq-cli\n");
    }

    #[test]
    fn split_installed_sizes_are_labelled_per_member() {
        let mut e = mk("bisq");
        e.pkgnames = vec![member("bisq-desktop", None), member("bisq-cli", None)];
        let x = Extras {
            installed: vec![
                InstalledMember {
                    name: PkgName::new("bisq-desktop"),
                    size: ByteSize::new(210 * 1024 * 1024),
                },
                InstalledMember {
                    name: PkgName::new("bisq-cli"),
                    size: ByteSize::new(1024 * 1024),
                },
            ],
            ..Default::default()
        };
        let out = render(&e, &x);
        assert_contains!(out, "Installed Size  : bisq-desktop: 210.00 MiB\n");
        assert_contains!(out, "                  bisq-cli: 1.00 MiB\n");
    }

    #[test]
    fn maintainer_comments_parse_case_insensitively_in_file_order() {
        let pkgbuild = "\
# maintainer: First <first@example.org>
#Maintainer : Second <second@example.org>
# Contributor: Old Hand <old@example.org>
# maintainership notes: not a person
pkgname=foo
# Maintainer: buried mid-file counts too
";
        assert_eq!(
            maintainers(pkgbuild),
            vec![
                Maintainer::new("First <first@example.org>"),
                Maintainer::new("Second <second@example.org>"),
                Maintainer::new("buried mid-file counts too"),
            ]
        );
    }

    #[test]
    fn maintainer_without_colon_or_value_is_skipped() {
        assert!(maintainers("# Maintainer\n# Maintainer:\n# Maintainer:   \n").is_empty());
    }

    /// The miss warning names the AUR only when its data was in play, and
    /// points at the calling surface's sync action otherwise.
    #[test]
    fn missing_warning_wording_follows_aur_state() {
        let foo = PkgTarget::new("foo");
        let bar = PkgTarget::new("bar");
        assert_eq!(
            missing_warning(AurState::Ready, &[&foo, &bar], "`aurox -Sy`"),
            "not in repos or AUR: foo, bar"
        );
        assert_eq!(
            missing_warning(AurState::NotSetUp, &[&foo], "`aurox -Sy`"),
            "not in repos: foo (no AUR index — `aurox -Sy` enables AUR lookups)"
        );
        assert_eq!(
            missing_warning(AurState::NotSetUp, &[&foo], "`refresh`"),
            "not in repos: foo (no AUR index — `refresh` enables AUR lookups)"
        );
        assert_eq!(
            missing_warning(AurState::Disabled, &[&foo], "`refresh`"),
            "not in repos: foo (AUR disabled: aur = false in config.toml)"
        );
    }

    #[test]
    fn every_label_fits_the_gutter() {
        for l in Label::ALL {
            assert!(
                l.text().len() <= GUTTER,
                "label {l:?} ({:?}) overflows the value column",
                l.text()
            );
        }
    }
}
