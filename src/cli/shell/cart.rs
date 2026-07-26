//! The staged transaction the shell builds up, run by `apply`.
//!
//! `add`/`drop`/`remove`/`clear` mutate a [`Cart`]; `upgrade` seeds it with the
//! available upgrades; `review`/`approve` move AUR items past the approval gate;
//! `apply` runs the whole thing in one go. None of it is persisted — quitting
//! drops the cart. `docs/RESOLUTION_FLOW.md` maps how a cart is resolved,
//! frozen, and executed.
//!
//! This module is the pure data model: staging, dedup, and the approval-state
//! transitions, all unit-tested here without I/O. The side effects the verbs
//! need (coarse repo/AUR classification, the PKGBUILD diff, the build+install)
//! live behind the [`super::ShellEnv`] trait.

use super::resolved::ResolvedCart;
use crate::build::{SourcePin, Target};
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName};
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::rc::Rc;

/// Where a staged install came from.
///
/// Decides auto-approval and how `show` labels the row. The *install routing*
/// (which `pacman` lane it takes) is re-decided by the resolver at apply time;
/// this tag only drives the approval policy and the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// In a sync repo — pacman owns its provenance, so it auto-approves.
    Repo,
    /// In the AUR index — has a PKGBUILD, so it needs review by default.
    Aur,
}

impl Source {
    /// Display label for the `show` table.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Aur => "aur",
        }
    }
}

impl From<Source> for SourcePin {
    /// The routing pin an explicit pick of this source carries into apply.
    fn from(s: Source) -> Self {
        match s {
            Source::Repo => Self::Repo,
            Source::Aur => Self::Aur,
        }
    }
}

/// Coarse staging classification of a name: the source lane plus, for repo
/// packages, the concrete sync-DB it lives in (`core`, `extra`, …).
///
/// The concrete `repo` is display-only — it drives the `show` table's repo
/// column and the `drop core`/`add extra` repo-filter selectors. The real
/// install routing is still the resolver's call at apply time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageClass {
    pub source: Source,
    /// Concrete sync-repo for [`Source::Repo`]; `None` for AUR.
    pub repo: Option<RepoName>,
}

/// How AUR packages enter the cart: needing review, or pre-approved.
///
/// The typed config value behind the `aur_approval` knob (a named type rather
/// than a bare bool, so a call site reads `AurApproval::Auto`, not `true`).
/// [`from_config`](Self::from_config) resolves the effective policy, including
/// the legacy `review_default == "skip"` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AurApproval {
    /// AUR items stage as [`Approval::NeedsReview`] (the default).
    #[default]
    Review,
    /// AUR items stage pre-approved.
    Auto,
}

impl AurApproval {
    /// The effective AUR approval policy. The explicit `aur_approval` config
    /// value wins when set; when unset (`None`) it defers to the legacy
    /// `review_default == "skip"` ⇒ [`Self::Auto`] behaviour so pre-`aur_approval`
    /// configs keep working. Everything else means review.
    pub fn from_config(configured: Option<Self>, review_default: &str) -> Self {
        match configured {
            Some(policy) => policy,
            None if review_default == "skip" => Self::Auto,
            None => Self::Review,
        }
    }
}

/// Whether a staged item still needs the user's eyes on its PKGBUILD before
/// `apply` will run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Cleared for `apply` (a repo package, or an AUR one the user approved).
    Approved,
    /// An AUR package the user hasn't reviewed/approved yet.
    NeedsReview,
}

impl Approval {
    /// The approval state a freshly-staged item gets, given its source and the
    /// AUR policy. Repo packages always auto-approve; AUR packages follow
    /// `aur`.
    pub const fn default_for(source: Source, aur: AurApproval) -> Self {
        match source {
            Source::Repo => Self::Approved,
            Source::Aur => match aur {
                AurApproval::Auto => Self::Approved,
                AurApproval::Review => Self::NeedsReview,
            },
        }
    }

    /// Display label for the `show` table.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::NeedsReview => "review",
        }
    }
}

/// Whether a cart item is part of the transaction, or a decision against it.
///
/// Orthogonal to [`Approval`], not a third value of it: this says *is it in
/// the transaction*, approval says *has the user cleared its PKGBUILD*. All
/// four combinations are real — an item you dropped after approving comes back
/// approved, one you dropped without looking comes back needing review — which
/// is why skipping doesn't overwrite the approval it would otherwise destroy.
/// The `show` table collapses the two into one column (a skipped item's
/// approval is moot until it's restaged), and that collapse is presentation,
/// so it lives in the renderer.
///
/// `drop` **marks** an item rather than deleting it: it keeps its place and
/// its number, so the numbered table doesn't renumber under a user working
/// down it, the choice stays visible instead of vanishing, and `add` on the
/// same item (by name or by its unchanged number) restores it. Only
/// [`Staged`](Self::Staged) items resolve, gate on approval, or reach `apply`
/// — see [`Cart::staged`] vs [`Cart::items`], the split that keeps what the
/// cart *holds* from being read as what it will *run*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staging {
    /// In the transaction.
    Staged,
    /// Dropped by the user: still held (and still numbered), excluded from
    /// everything else.
    Skipped,
}

impl Staging {
    /// Display label for the `show` table's state column.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Skipped => "skipped",
        }
    }

    /// In the transaction?
    pub const fn is_staged(self) -> bool {
        matches!(self, Self::Staged)
    }
}

/// One staged install/upgrade: the target plus the bookkeeping the cart tracks.
#[derive(Debug, Clone)]
pub struct CartItem {
    /// Carries the counterpart hint through expand → resolve → prepare, exactly
    /// like the upgrade loop. Fresh `add` items are unhinted; `upgrade` seeds
    /// hinted ones (the foreign pkgname).
    pub target: Target,
    pub source: Source,
    pub approval: Approval,
    /// Concrete sync-repo (`core`, `extra`, …) for a repo package; `None` for
    /// AUR rows and for repo rows staged before the source DB was known. Drives
    /// the `show` table's repo column and the `drop core` repo filter — see
    /// [`Self::repo_label`].
    pub repo: Option<RepoName>,
    /// `Some` when this row came from `upgrade` — it carries the old→new
    /// versions for the `show` table and routes repo rows through the partial
    /// `pacman -Syu` lane at apply (rather than a fresh `pacman -S`).
    pub upgrade: Option<PkgUpgrade>,
    /// In the transaction, or dropped by the user and kept on screen. See
    /// [`Staging`].
    pub staging: Staging,
}

impl CartItem {
    /// Stage a fresh install of `target` from `source`, defaulting the approval
    /// per `source` + the AUR policy. `repo` is the concrete sync-DB for a repo
    /// package (display only). The build pipeline's [`Target`] starts unhinted —
    /// `resolver::expand_pkgbase_targets` infers a counterpart hint on rewrite
    /// if needed.
    pub fn new(
        target: PkgTarget,
        source: Source,
        repo: Option<RepoName>,
        aur: AurApproval,
    ) -> Self {
        Self {
            target: Target::bare(target),
            source,
            approval: Approval::default_for(source, aur),
            repo,
            upgrade: None,
            staging: Staging::Staged,
        }
    }

    /// Stage an upgrade candidate (from `upgrade`). The source follows the
    /// candidate's repo (`REPO_AUR` ⇒ AUR), and AUR rows hint their foreign
    /// pkgname so the counterpart resolves to the right installed pkg — exactly
    /// what the loop's `resolve_aur` did.
    pub fn from_upgrade(u: PkgUpgrade, aur: AurApproval) -> Self {
        let source = if u.repo == REPO_AUR {
            Source::Aur
        } else {
            Source::Repo
        };
        let target = match source {
            Source::Aur => Target::with_hint(&u.name, u.name.clone()),
            Source::Repo => Target::bare(&u.name),
        };
        // AUR rows label as `aur` from the source; a repo row carries its
        // concrete sync-DB so the table shows `core`/`extra`/… not just `repo`.
        let repo = (source == Source::Repo).then(|| u.repo.clone());
        Self {
            target,
            source,
            approval: Approval::default_for(source, aur),
            repo,
            upgrade: Some(u),
            staging: Staging::Staged,
        }
    }

    /// The freeform user-typed spec this item stages — the item's identity
    /// within the cart.
    pub const fn spec(&self) -> &PkgTarget {
        &self.target.spec
    }

    /// The repo bucket this row displays in and a repo filter matches against:
    /// the concrete sync-DB for a known repo package, `aur` for an AUR row, or
    /// `repo` when a repo package was staged before its source DB was resolved.
    pub fn repo_label(&self) -> RepoName {
        match (self.source, &self.repo) {
            (Source::Aur, _) => RepoName::from(REPO_AUR),
            (Source::Repo, Some(r)) => r.clone(),
            (Source::Repo, None) => RepoName::from("repo"),
        }
    }

    /// A repo *upgrade* row — applied via the partial `pacman -Syu` lane, not a
    /// fresh `pacman -S`.
    pub fn is_repo_upgrade(&self) -> bool {
        self.source == Source::Repo && self.upgrade.is_some()
    }

    /// Still in the transaction — the filter every "what runs" query applies.
    pub const fn is_staged(&self) -> bool {
        self.staging.is_staged()
    }

    /// `old → new` for an upgrade row, `None` for a fresh install (for `show`).
    pub fn version_transition(&self) -> Option<String> {
        self.upgrade
            .as_ref()
            .map(|u| format!("{} → {}", u.old_ver, u.new_ver))
    }
}

/// What one `apply` run reports back to the dispatch core.
///
/// The outcome, plus the review set as the run left it — the cart's set
/// extended by any PKGBUILD the user approved *during* the run (pulled-in
/// AUR dependencies prompt mid-build). The env reads the cart; the core owns
/// folding this knowledge back in ([`Cart::absorb_reviewed`]), on **every**
/// outcome, so a diff approved before a failure isn't re-prompted on the
/// retry. (An `Err` abort carries no data and still loses mid-run approvals
/// — the accepted limit of the seam.)
#[derive(Debug)]
pub struct ApplyRun {
    pub outcome: ApplyOutcome,
    pub reviewed: HashSet<PkgBase>,
}

/// The outcome the dispatch core uses to update the cart after `env.apply`.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// User declined at the sysupgrade-preflight override gate — the cart is
    /// left untouched. (The explicit `do` command is itself the transaction
    /// consent; there is no general confirm to decline at.)
    Declined,
    /// Everything installed/removed cleanly — the applied rows leave the cart.
    Succeeded,
    /// The transaction ran but something failed or was interrupted. `installed`
    /// carries the staged install rows that *did* land, so the cart drops them
    /// and keeps only the offenders — the ones still to `drop`/fix and retry.
    /// Staged removals stay put (they don't run once a build fails). Empty when
    /// nothing landed at all.
    Failed { installed: Vec<PkgTarget> },
}

/// What staging an item (`add` / `stage_remove`) did to the cart — a named
/// outcome rather than a bare `bool` so the call site reads as intent and pairs
/// with [`ApproveResult`] / [`UnstageResult`].
#[derive(Debug, PartialEq, Eq)]
pub enum StageResult {
    /// The item was newly staged.
    Staged,
    /// An item the user had dropped is back in the transaction, in place — the
    /// undo half of [`Staging::Skipped`].
    Restaged,
    /// The spec was already staged — re-staging is an idempotent no-op.
    AlreadyStaged,
}

/// What `drop` (`unstage`) did to the cart.
#[derive(Debug, PartialEq, Eq)]
pub enum UnstageResult {
    /// A staged item is now marked skipped — still held, out of the
    /// transaction.
    Unstaged,
    /// The item was already skipped; nothing changed.
    AlreadySkipped,
    /// Nothing in the cart matched the target.
    NotStaged,
}

/// What `keep` did to the cart — the set-complement of [`UnstageResult`], where
/// `drop` names the rows to remove and `keep` names the rows to spare.
#[derive(Debug, PartialEq, Eq)]
pub enum KeepResult {
    /// No staged install matched the keep-set — the cart is left untouched, so a
    /// mistyped `keep` can't silently empty it.
    NoMatch,
    /// Kept the matched rows and dropped the rest; carries the dropped specs (in
    /// cart order) as [`PkgTarget`]s for the caller to report. Empty when the
    /// keep-set already covered every staged install (a no-op `keep`).
    Kept { dropped: Vec<PkgTarget> },
}

/// What `approve <spec>` did to a staged item — so the caller can report it and
/// know whether it newly cleared the gate (and should record the pkgbase as
/// reviewed).
#[derive(Debug, PartialEq, Eq)]
pub enum ApproveResult {
    /// The spec isn't in the cart.
    NotStaged,
    /// It was already approved — nothing changed.
    AlreadyApproved,
    /// It moved from `NeedsReview` to `Approved`.
    Approved,
}

/// What one `review` of an AUR pkgbase decided.
#[derive(Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// User approved the PKGBUILD — clear the item for `apply`.
    Approved,
    /// User chose "approve all" — clear this item *and* every remaining one in
    /// the pass without opening another diff.
    ApprovedAll,
    /// User looked but deferred — the item stays `NeedsReview`.
    Skipped,
    /// User aborted the whole review pass — stop, leave the rest as they are.
    Aborted,
}

/// The pending transaction. Built across many commands, run by `apply`.
///
/// `Clone` backs the shell's `undo` stack: each cart-changing command snapshots
/// the pre-change cart, and `undo` restores the top snapshot. The frozen
/// [`ResolvedCart`] rides *inside* the cart (behind an `Rc`, so the snapshot
/// clone is cheap) so undo restores the roots and their resolution together.
#[derive(Default, Clone)]
pub struct Cart {
    /// Staged installs/upgrades (repo + AUR), each with its approval state.
    items: Vec<CartItem>,
    /// Packages staged for uninstall → `pacman -R` at apply.
    remove: Vec<PkgName>,
    /// PKGBUILDs approved this session, keyed by pkgbase — threaded into the
    /// build pipeline so it doesn't re-prompt a diff the user already cleared
    /// in the shell (survives discard/re-add and post-failure retries).
    reviewed: HashSet<PkgBase>,
    /// The whole-cart transaction resolved at the last `add`/`upgrade`/`drop`/…
    /// — what `show` renders and `apply` executes, with no re-resolution.
    /// `None` before the first resolve and after a `clear`. Every install-set
    /// change replaces it (or, on a rejected `add`, leaves it untouched).
    resolution: Option<Rc<ResolvedCart>>,
}

impl Cart {
    /// Nothing left to run: no *staged* install and no removal. Skipped items
    /// don't count — they're a decision on screen, not work to do — so a cart
    /// whose items were all dropped is empty to `apply` while [`Self::items`]
    /// still holds them. Ask `items().is_empty()` for "nothing to show".
    pub fn is_empty(&self) -> bool {
        !self.items.iter().any(CartItem::is_staged) && self.remove.is_empty()
    }

    /// **Every** item, skipped ones included, in cart order — one row per item
    /// is what `show` renders and what the numbered referent snapshots, so an
    /// item keeps its number when it's dropped and `add <that number>`
    /// restores it.
    ///
    /// The counterpart of [`Self::staged`]: this is what the cart *holds*,
    /// that is what it will *run*. Nothing that resolves, gates, or installs
    /// may read this one.
    pub fn items(&self) -> &[CartItem] {
        &self.items
    }

    /// The items still in the transaction — everything that resolves, gates
    /// on approval, or reaches `apply`. See [`Staging`].
    pub fn staged(&self) -> impl Iterator<Item = &CartItem> {
        self.items.iter().filter(|i| i.is_staged())
    }

    /// How many items are in the transaction — the count the header prints.
    pub fn staged_len(&self) -> usize {
        self.staged().count()
    }

    /// How many items the user dropped — the header's trailing `N skipped`.
    pub fn skipped_len(&self) -> usize {
        self.items.len() - self.staged_len()
    }

    /// The staged removals, in staging order.
    pub fn removals(&self) -> &[PkgName] {
        &self.remove
    }

    /// Pkgbases already reviewed this session — fed to the build pipeline to
    /// suppress repeat diffs.
    pub const fn reviewed(&self) -> &HashSet<PkgBase> {
        &self.reviewed
    }

    /// The frozen whole-cart resolution, or `None` before the first resolve /
    /// after a clear. `show` renders it; `apply` executes it. `pub(super)` — it
    /// hands back the crate-private [`ResolvedCart`], so it stays inside the
    /// shell module.
    pub(super) const fn resolution(&self) -> Option<&Rc<ResolvedCart>> {
        self.resolution.as_ref()
    }

    /// Replace the frozen resolution — the one write, at the end of every
    /// install-set change once the new set resolved. `Rc` so the undo snapshot
    /// (a full cart clone) stays cheap.
    pub(super) fn set_resolution(&mut self, resolved: Rc<ResolvedCart>) {
        self.resolution = Some(resolved);
    }

    /// Stage one install. Returns `false` (and stages nothing) when the spec is
    /// already in the cart — re-`add`ing is idempotent, not a duplicate row.
    ///
    /// Inserts keeping [`Self::items`] sorted (repo-rank → repo → name) so the
    /// table renders stably grouped however the cart was assembled. Number
    /// resolution no longer reads this vector live — `show` snapshots the
    /// rendered rows into the referent (see `NumberedList` in the shell root),
    /// so numbers stay bound to what was printed even across a re-sort.
    pub fn add(&mut self, item: CartItem) -> StageResult {
        match self.items.iter_mut().find(|i| i.spec() == item.spec()) {
            // Restore in place rather than replacing: the existing item may be
            // an `upgrade`-seeded one, whose `PkgUpgrade` (the old→new pair and
            // the partial `-Syu` routing) a freshly-built `add` item wouldn't
            // carry. So `drop N` then `add N` is a true undo, not a re-add of
            // a lesser item.
            Some(row) if !row.is_staged() => {
                row.staging = Staging::Staged;
                StageResult::Restaged
            }
            Some(_) => StageResult::AlreadyStaged,
            None => {
                self.items.push(item);
                self.sort_items();
                StageResult::Staged
            }
        }
    }

    /// Re-establish the cart's sort invariant: rows grouped by repo
    /// (repo-rank → concrete repo name), then by spec within a repo — the same
    /// order the unified `show` table renders. The cart is tiny, so a full
    /// re-sort per `add` is cheaper than threading a sorted-insert position.
    /// `unstage` / `approve` / `clear_applied` preserve relative order, so only
    /// the inserting paths need this.
    fn sort_items(&mut self) {
        self.items.sort_by(|a, b| {
            let (ra, rb) = (a.repo_label(), b.repo_label());
            ra.rank()
                .cmp(&rb.rank())
                .then_with(|| ra.as_str().cmp(rb.as_str()))
                .then_with(|| a.spec().cmp(b.spec()))
        });
    }

    /// Drop an install: **mark** the item skipped, keeping it (and its number)
    /// in place. See [`Staging`] for why this isn't a removal.
    pub fn unstage(&mut self, target: &PkgTarget) -> UnstageResult {
        match self.items.iter_mut().find(|i| i.spec() == target.as_str()) {
            None => UnstageResult::NotStaged,
            Some(row) if !row.is_staged() => UnstageResult::AlreadySkipped,
            Some(row) => {
                row.staging = Staging::Skipped;
                UnstageResult::Unstaged
            }
        }
    }

    /// Remove an item outright — the one path that still deletes.
    ///
    /// `apply` uses it for the items that actually landed: an installed
    /// package is *done*, not a decision to keep showing, so it leaves the
    /// cart while the offenders (and the user's skips) stay for the retry.
    pub fn remove_applied(&mut self, target: &PkgTarget) {
        self.items.retain(|i| i.spec() != target.as_str());
    }

    /// Keep only the staged installs whose spec is in `keep`, dropping every
    /// other install row — the inverse of [`Self::unstage`]: `drop` names the
    /// rows to remove, `keep` names the rows to spare (handy for narrowing a
    /// large `upgrade`-seeded cart down to a few packages).
    ///
    /// Removals are left untouched — `keep` mirrors `drop`, which only unstages
    /// installs. Guards against emptying the cart on a typo: when no staged
    /// install matches, returns [`KeepResult::NoMatch`] and changes nothing.
    /// Relative order of the kept rows is preserved, so the sorted-cart
    /// invariant holds without a re-sort.
    pub fn keep<'a>(&mut self, keep: impl IntoIterator<Item = &'a PkgTarget>) -> KeepResult {
        // Fully typed: an item's identity (`spec()`) is a `PkgTarget`, so
        // the membership probes below never leave target space.
        let keep: HashSet<PkgTarget> = keep.into_iter().cloned().collect();
        if !self.staged().any(|i| keep.contains(i.spec())) {
            return KeepResult::NoMatch;
        }
        let mut dropped = Vec::new();
        for row in &mut self.items {
            if keep.contains(row.spec()) {
                continue;
            }
            // Already-skipped rows aren't dropped *again* — `keep` reports what
            // it changed, so a second `keep` is a quiet no-op on them.
            if row.is_staged() {
                dropped.push(row.spec().clone());
                row.staging = Staging::Skipped;
            }
        }
        KeepResult::Kept { dropped }
    }

    /// Fold an [`ApplyRun`]'s review knowledge back in — a set union, so the
    /// incoming set's iteration order is irrelevant.
    pub fn absorb_reviewed(&mut self, reviewed: HashSet<PkgBase>) {
        self.reviewed.extend(reviewed);
    }

    /// Stage a removal (uninstall). [`StageResult::AlreadyStaged`] when it was
    /// already staged for removal.
    pub fn stage_remove(&mut self, name: PkgName) -> StageResult {
        if self.remove.contains(&name) {
            return StageResult::AlreadyStaged;
        }
        self.remove.push(name);
        StageResult::Staged
    }

    /// Empty everything — installs, removals, the reviewed set, and the frozen
    /// resolution.
    pub fn clear(&mut self) {
        self.items.clear();
        self.remove.clear();
        self.reviewed.clear();
        self.resolution = None;
    }

    /// Drop the installs + removals after a clean `apply`, but keep the
    /// reviewed set so a later re-`add` of the same pkgbase isn't re-prompted.
    /// The resolution described the just-applied set, so it goes too.
    pub fn clear_applied(&mut self) {
        self.items.clear();
        self.remove.clear();
        self.resolution = None;
    }

    /// The staged item matching `target`, if any. Skipped rows are invisible
    /// here: `review`/`approve` act on the transaction, not on what's merely
    /// still listed.
    pub fn item(&self, target: &PkgTarget) -> Option<&CartItem> {
        self.staged().find(|i| i.spec() == target.as_str())
    }

    /// Record that `pkgbase`'s PKGBUILD was reviewed this session.
    pub fn mark_reviewed(&mut self, pkgbase: PkgBase) {
        self.reviewed.insert(pkgbase);
    }

    /// Approve the staged item for `target`, reporting what changed. The caller
    /// records the pkgbase as reviewed only on [`ApproveResult::Approved`].
    pub fn approve(&mut self, target: &PkgTarget) -> ApproveResult {
        match self
            .items
            .iter_mut()
            .find(|i| i.is_staged() && i.spec() == target.as_str())
        {
            None => ApproveResult::NotStaged,
            Some(i) if i.approval == Approval::Approved => ApproveResult::AlreadyApproved,
            Some(i) => {
                i.approval = Approval::Approved;
                ApproveResult::Approved
            }
        }
    }

    /// The AUR items still blocking `apply` — those that haven't been approved.
    pub fn pending_review(&self) -> Vec<&CartItem> {
        self.staged()
            .filter(|i| i.approval == Approval::NeedsReview)
            .collect()
    }

    /// Whether every staged item is cleared for `apply`.
    pub fn all_approved(&self) -> bool {
        self.staged().all(|i| i.approval == Approval::Approved)
    }

    /// The targets the install/build half of `apply` resolves through the `-S`
    /// pipeline: AUR rows (install or upgrade) and fresh repo installs. Repo
    /// *upgrades* are excluded — they go through the partial `pacman -Syu` lane
    /// ([`Self::repo_upgrades`]).
    pub fn install_targets(&self) -> Vec<Target> {
        self.staged()
            .filter(|i| !i.is_repo_upgrade())
            .map(|i| i.target.clone())
            .collect()
    }

    /// The staged repo *upgrade* rows, applied via `pacman -Syu` (ignoring every
    /// repo upgrade candidate the user didn't stage).
    pub fn repo_upgrades(&self) -> Vec<&PkgUpgrade> {
        self.staged()
            .filter(|i| i.is_repo_upgrade())
            .filter_map(|i| i.upgrade.as_ref())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(spec: &str, source: Source) -> CartItem {
        CartItem::new(PkgTarget::new(spec), source, None, AurApproval::Review)
    }

    fn target(spec: &str) -> PkgTarget {
        PkgTarget::new(spec)
    }

    #[test]
    fn repo_items_auto_approve_aur_items_need_review() {
        assert_eq!(item("glibc", Source::Repo).approval, Approval::Approved);
        assert_eq!(item("yay-bin", Source::Aur).approval, Approval::NeedsReview);
    }

    #[test]
    fn aur_auto_approve_policy_skips_review() {
        let it = CartItem::new(target("yay-bin"), Source::Aur, None, AurApproval::Auto);
        assert_eq!(it.approval, Approval::Approved);
    }

    #[test]
    fn aur_approval_from_config_prefers_the_explicit_knob() {
        // Explicit `aur_approval` wins, regardless of `review_default`.
        assert_eq!(
            AurApproval::from_config(Some(AurApproval::Auto), "prompt"),
            AurApproval::Auto
        );
        assert_eq!(
            AurApproval::from_config(Some(AurApproval::Review), "skip"),
            AurApproval::Review
        );
    }

    #[test]
    fn aur_approval_from_config_falls_back_to_review_default() {
        // Unset → legacy behaviour: `review_default == "skip"` auto-approves,
        // anything else needs review.
        assert_eq!(AurApproval::from_config(None, "skip"), AurApproval::Auto);
        assert_eq!(
            AurApproval::from_config(None, "prompt"),
            AurApproval::Review
        );
        assert_eq!(
            AurApproval::from_config(None, "always-show"),
            AurApproval::Review
        );
    }

    #[test]
    fn aur_approval_deserializes_from_lowercase_toml() {
        // The config knob accepts the lowercase variant names, parsed the same
        // way `Config` reads the field (a table key, not a bare value).
        #[derive(Deserialize)]
        struct Knob {
            #[serde(default)]
            aur_approval: Option<AurApproval>,
        }
        let auto: Knob = toml::from_str("aur_approval = \"auto\"").unwrap();
        assert_eq!(auto.aur_approval, Some(AurApproval::Auto));
        let review: Knob = toml::from_str("aur_approval = \"review\"").unwrap();
        assert_eq!(review.aur_approval, Some(AurApproval::Review));
        // A missing key is the unset (`None`) tri-state, not an error.
        let empty: Knob = toml::from_str("").unwrap();
        assert_eq!(empty.aur_approval, None);
    }

    #[test]
    fn add_dedups_by_spec() {
        let mut cart = Cart::default();
        assert_eq!(cart.add(item("foo", Source::Aur)), StageResult::Staged);
        assert_eq!(
            cart.add(item("foo", Source::Aur)),
            StageResult::AlreadyStaged,
            "re-add is a no-op"
        );
        assert_eq!(cart.items().len(), 1);
    }

    /// `drop` marks: the item keeps its place (and so its number) and leaves
    /// the transaction. A second `drop` on it reports the state rather than a
    /// miss, and `add` puts it back where it was.
    #[test]
    fn unstage_marks_skipped_and_keeps_the_item() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        assert_eq!(cart.unstage(&target("foo")), UnstageResult::Unstaged);
        assert_eq!(
            cart.unstage(&target("foo")),
            UnstageResult::AlreadySkipped,
            "a second drop finds the row, already skipped"
        );
        let listed: Vec<&str> = cart.items().iter().map(|i| i.spec().as_str()).collect();
        assert_eq!(listed, vec!["bar", "foo"], "both items still held");
        let staged: Vec<&str> = cart.staged().map(|i| i.spec().as_str()).collect();
        assert_eq!(staged, vec!["bar"], "only `bar` is in the transaction");
        assert_eq!(cart.staged_len(), 1);
        assert_eq!(cart.skipped_len(), 1);

        // Re-adding restores it in place — same row, not an appended one.
        assert_eq!(cart.add(item("foo", Source::Aur)), StageResult::Restaged);
        let staged: Vec<&str> = cart.staged().map(|i| i.spec().as_str()).collect();
        assert_eq!(staged, vec!["bar", "foo"]);
    }

    /// An `upgrade`-seeded item survives the drop/restore round-trip *with* its
    /// `PkgUpgrade` — the reason `add` restores in place instead of replacing
    /// it with a freshly-built one, which would carry no old→new pair and
    /// would lose the partial `-Syu` routing.
    #[test]
    fn restaging_an_upgrade_row_keeps_its_upgrade() {
        let mut cart = Cart::default();
        cart.add(CartItem::from_upgrade(
            upgrade("core", "glibc"),
            AurApproval::Review,
        ));
        assert_eq!(cart.unstage(&target("glibc")), UnstageResult::Unstaged);
        assert_eq!(
            cart.add(CartItem::new(
                PkgTarget::new("glibc"),
                Source::Repo,
                None,
                AurApproval::Review
            )),
            StageResult::Restaged
        );
        let row = &cart.items()[0];
        assert!(row.is_staged());
        assert!(row.upgrade.is_some(), "the old→new pair survived");
        assert!(row.is_repo_upgrade(), "still routed through the -Syu lane");
    }

    /// A cart whose every item is skipped has nothing to run — `apply` sees it
    /// as empty — while the items stay held for `show` to render.
    #[test]
    fn all_skipped_is_empty_to_apply_but_still_has_items() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.unstage(&target("foo"));
        assert!(cart.is_empty(), "nothing staged to run");
        assert_eq!(cart.items().len(), 1, "still on screen");
        assert!(cart.all_approved(), "no staged item gates the transaction");
        assert!(cart.pending_review().is_empty());
        assert!(cart.install_targets().is_empty());
    }

    fn keep_targets(specs: &[&str]) -> Vec<PkgTarget> {
        specs.iter().map(|s| PkgTarget::new(*s)).collect()
    }

    #[test]
    fn keep_drops_everything_but_the_selected() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        cart.add(item("baz", Source::Aur));
        // Keep only `bar` — the two AUR rows drop, reported in cart order.
        assert_eq!(
            cart.keep(&keep_targets(&["bar"])),
            KeepResult::Kept {
                dropped: vec![PkgTarget::new("baz"), PkgTarget::new("foo")]
            }
        );
        let staged: Vec<&str> = cart.staged().map(|i| i.spec().as_str()).collect();
        assert_eq!(staged, vec!["bar"], "only the kept item runs");
        let listed: Vec<&str> = cart.items().iter().map(|i| i.spec().as_str()).collect();
        assert_eq!(
            listed,
            vec!["bar", "baz", "foo"],
            "the dropped items stay held, numbered as before"
        );
    }

    #[test]
    fn keep_matching_nothing_leaves_the_cart_intact() {
        // A keep-set that hits no staged row must not empty the cart (typo guard).
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        assert_eq!(cart.keep(&keep_targets(&["absent"])), KeepResult::NoMatch);
        assert_eq!(cart.items().len(), 2, "nothing dropped on no match");
    }

    #[test]
    fn keep_covering_the_whole_cart_drops_nothing() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        // Every staged row is kept → a no-op, with an empty dropped list.
        assert_eq!(
            cart.keep(&keep_targets(&["foo", "bar"])),
            KeepResult::Kept {
                dropped: Vec::new()
            }
        );
        assert_eq!(cart.items().len(), 2);
    }

    #[test]
    fn keep_leaves_removals_untouched() {
        // `keep` mirrors `drop` — it acts on installs only, not staged removals.
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.stage_remove(PkgName::from("old"));
        assert!(matches!(
            cart.keep(&keep_targets(&["foo"])),
            KeepResult::Kept { .. }
        ));
        assert_eq!(cart.removals(), &[PkgName::from("old")]);
    }

    #[test]
    fn stage_remove_dedups() {
        let mut cart = Cart::default();
        assert_eq!(cart.stage_remove(PkgName::from("old")), StageResult::Staged);
        assert_eq!(
            cart.stage_remove(PkgName::from("old")),
            StageResult::AlreadyStaged
        );
        assert_eq!(cart.removals().len(), 1);
    }

    #[test]
    fn gate_blocks_until_aur_items_approved() {
        let mut cart = Cart::default();
        cart.add(item("glibc", Source::Repo));
        cart.add(item("yay-bin", Source::Aur));
        assert!(!cart.all_approved());
        assert_eq!(cart.pending_review().len(), 1);
        assert_eq!(cart.pending_review()[0].spec(), "yay-bin");

        cart.approve(&target("yay-bin"));
        assert!(cart.all_approved());
        assert!(cart.pending_review().is_empty());
    }

    #[test]
    fn approve_reports_the_transition() {
        let mut cart = Cart::default();
        cart.add(item("yay-bin", Source::Aur));
        assert_eq!(cart.approve(&target("yay-bin")), ApproveResult::Approved);
        assert_eq!(
            cart.approve(&target("yay-bin")),
            ApproveResult::AlreadyApproved
        );
        assert_eq!(cart.approve(&target("absent")), ApproveResult::NotStaged);
        assert!(cart.all_approved());
    }

    #[test]
    fn repo_only_cart_is_immediately_approved() {
        let mut cart = Cart::default();
        cart.add(item("glibc", Source::Repo));
        assert!(cart.all_approved());
    }

    #[test]
    fn clear_empties_everything_including_reviewed() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.stage_remove(PkgName::from("old"));
        cart.mark_reviewed(PkgBase::from("foo"));
        cart.clear();
        assert!(cart.is_empty());
        assert!(cart.reviewed().is_empty());
    }

    #[test]
    fn clear_applied_keeps_reviewed() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.stage_remove(PkgName::from("old"));
        cart.mark_reviewed(PkgBase::from("foo"));
        cart.clear_applied();
        assert!(cart.is_empty());
        assert!(
            cart.reviewed().contains(&PkgBase::from("foo")),
            "reviewed set survives a clean apply"
        );
    }

    #[test]
    fn install_targets_lists_every_staged_spec() {
        let mut cart = Cart::default();
        cart.add(item("foo", Source::Aur));
        cart.add(item("bar", Source::Repo));
        let targets = cart.install_targets();
        let specs: Vec<&str> = targets.iter().map(|t| t.spec.as_str()).collect();
        // Sorted-cart invariant: `bar` (repo, ranks before AUR) precedes `foo`
        // (aur, sorts last) regardless of staging order.
        assert_eq!(specs, vec!["bar", "foo"]);
    }

    #[test]
    fn add_keeps_items_sorted_by_repo_then_name() {
        let mut cart = Cart::default();
        // Stage in deliberately scrambled order across repos.
        cart.add(CartItem::from_upgrade(
            upgrade("aur", "yay-bin"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("extra", "vim"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("core", "zlib"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("core", "glibc"),
            AurApproval::Review,
        ));
        // core (alphabetical within repo) → extra → aur last.
        let order: Vec<&str> = cart.items().iter().map(|i| i.spec().as_str()).collect();
        assert_eq!(order, vec!["glibc", "zlib", "vim", "yay-bin"]);
    }

    fn upgrade(repo: &str, name: &str) -> PkgUpgrade {
        use crate::version::Version;
        PkgUpgrade {
            repo: RepoName::from(repo),
            name: PkgName::from(name),
            old_ver: Version::from("1-1"),
            new_ver: Version::from("2-1"),
        }
    }

    #[test]
    fn from_upgrade_tags_source_and_hint() {
        let aur = CartItem::from_upgrade(upgrade("aur", "yay-bin"), AurApproval::Review);
        assert_eq!(aur.source, Source::Aur);
        assert_eq!(aur.approval, Approval::NeedsReview);
        assert_eq!(
            aur.target.hint.as_ref().map(PkgName::as_str),
            Some("yay-bin")
        );
        assert!(!aur.is_repo_upgrade());
        // AUR rows label `aur` from the source, not a stored concrete repo.
        assert_eq!(aur.repo, None);
        assert_eq!(aur.repo_label(), "aur");

        let repo = CartItem::from_upgrade(upgrade("core", "glibc"), AurApproval::Review);
        assert_eq!(repo.source, Source::Repo);
        assert_eq!(repo.approval, Approval::Approved);
        assert!(repo.is_repo_upgrade());
        assert_eq!(repo.version_transition().as_deref(), Some("1-1 → 2-1"));
        // A repo row carries its concrete sync-DB for the table's repo column.
        assert_eq!(repo.repo_label(), "core");
    }

    #[test]
    fn repo_label_falls_back_when_source_db_unknown() {
        // A repo package staged without a concrete repo (e.g. a fresh `add`
        // before classification surfaced the DB) still labels as `repo`.
        assert_eq!(item("glibc", Source::Repo).repo_label(), "repo");
        // AUR always labels `aur`.
        assert_eq!(item("yay-bin", Source::Aur).repo_label(), "aur");
    }

    #[test]
    fn repo_upgrades_split_from_install_targets() {
        let mut cart = Cart::default();
        cart.add(CartItem::from_upgrade(
            upgrade("core", "glibc"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            upgrade("aur", "yay-bin"),
            AurApproval::Review,
        ));
        cart.add(item("firefox", Source::Repo)); // a fresh repo install
        // Repo upgrades take the -Syu lane; the rest take the -S/build pipeline.
        assert_eq!(
            cart.repo_upgrades()
                .iter()
                .map(|u| u.name.as_str())
                .collect::<Vec<_>>(),
            vec!["glibc"]
        );
        // Sorted-cart invariant: firefox (repo, ranks before AUR) precedes
        // yay-bin (aur, sorts last).
        let install: Vec<PkgTarget> = cart.install_targets().into_iter().map(|t| t.spec).collect();
        assert_eq!(install, vec!["firefox", "yay-bin"]);
    }
}
