//! Consent gate for the AUR bootstrap clone.
//!
//! The first AUR touch needs a full clone of the AUR monorepo — a ~2 GiB
//! download that takes ~8–9 minutes — and `-Syy` re-downloads it on purpose.
//! Neither ever starts silently: every path that could trigger a bootstrap
//! funnels through [`plan`], which announces the cost and asks first.
//! Incremental fetches of an existing mirror never prompt, and `aur = false`
//! in config.toml (pacman-only mode) skips the AUR half — prompts included.
//!
//! The gate is two pure decisions run in sequence. First, what the AUR half
//! *wants* ([`decide`]):
//!
//! | `aur` cfg | mirror on disk | trigger         | wants                          |
//! |-----------|----------------|-----------------|--------------------------------|
//! | off       | any            | any             | skip — pacman-only, no prompt  |
//! | on        | ready          | `-Syy`          | bootstrap (forced re-clone)    |
//! | on        | ready          | anything else   | incremental fetch, no prompt   |
//! | on        | interrupted    | any             | bootstrap (redo from scratch)  |
//! | on        | absent         | any             | bootstrap (first run)          |
//!
//! Second, how a wanted bootstrap obtains consent ([`consent_mode`]);
//! "explicit" is a command whose point is the refresh (`-Sy`/`-Syy`, shell
//! `refresh`/`upgrade`), "implicit" is the schema-bump resync acting on the
//! user's behalf:
//!
//! | `--noconfirm` | trigger  | stdin a TTY | consent                                     |
//! |---------------|----------|-------------|---------------------------------------------|
//! | yes           | any      | any         | auto-yes: announce, proceed                 |
//! | no            | explicit | yes         | announce + Y/n prompt (default yes)         |
//! | no            | explicit | no          | announce + read line: EOF ⇒ yes, `n` ⇒ no   |
//! | no            | implicit | yes         | announce + Y/n prompt (default yes)         |
//! | no            | implicit | no          | refuse — never bootstrap behind a pipe      |
//!
//! A decline or refusal still refreshes the official sync DBs, still records
//! the fetch-TTL stamp (so a TTL-driven `upgrade` doesn't re-prompt within
//! the window), and surfaces as [`RefreshOutcome::AurSkipped`] with its
//! [`SkipCause`] so every caller can word what was skipped.

use crate::config::Config;
use crate::error::Result;
use crate::paths;
use crate::runopts;
use crate::ui;
use std::fmt;
use std::io::IsTerminal;
use std::path::Path;
use tracing::info;

/// Who asked for this refresh — picks the `-Syy` force-reclone behaviour and
/// how consent for a needed bootstrap is obtained (see [`consent_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    /// `aurox -Sy` — explicit CLI refresh.
    ExplicitSync,
    /// `aurox -Syy` — explicit forced re-clone (wipes the mirror first).
    ForceReclone,
    /// Shell `refresh` / `upgrade` — explicit shell command.
    Shell,
    /// [`crate::index::load_or_resync`]'s schema-bump rebuild — implicit: the
    /// user typed something unrelated (`-Ss`, `-S`, …), so a non-interactive
    /// run must never bootstrap on its behalf.
    IndexResync,
}

impl RefreshReason {
    /// Whether the user's own command asked for the refresh (vs. an implicit
    /// trigger acting on their behalf).
    const fn is_explicit(self) -> bool {
        !matches!(self, Self::IndexResync)
    }
}

/// What one [`super::cmd_refresh`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// AUR mirror + index are current (bootstrap, incremental, or a no-op fetch).
    Refreshed,
    /// The AUR half was skipped; the official sync DBs were still refreshed.
    AurSkipped(SkipCause),
}

/// Why the AUR half of a refresh was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipCause {
    /// `aur = false` in config.toml — pacman-only mode.
    Disabled,
    /// The user answered "n" to the bootstrap prompt.
    Declined,
    /// An implicit trigger needed a bootstrap but had no terminal to ask on.
    NonInteractive,
}

impl fmt::Display for SkipCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disabled => "aur = false in config",
            Self::Declined => "declined",
            Self::NonInteractive => "non-interactive run",
        })
    }
}

/// The resolved fate of the AUR half of one refresh. [`decide`] produces it
/// with `Bootstrap` meaning "wants a bootstrap"; [`plan`] applies the consent
/// step, so a `Bootstrap` returned from there is already approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AurAction {
    /// Incremental fetch of the existing mirror.
    Fetch,
    /// Full clone + index rebuild from scratch.
    Bootstrap(BootstrapKind),
    /// Leave the AUR mirror alone (the repo-db sync still runs).
    Skip(SkipCause),
}

/// Which flavour of full clone is about to run — picks the announcement copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BootstrapKind {
    /// No mirror on disk yet.
    FirstRun,
    /// A previous bootstrap died before writing refs; redo from scratch.
    InterruptedRedo,
    /// `-Syy`: a healthy mirror is deliberately re-cloned.
    ForcedReclone,
}

/// On-disk state of the mirror, per [`super::is_bootstrapped`]'s artifact rule
/// (refs exist ⇔ the clone finished).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorState {
    /// Bootstrapped and usable — refreshes are incremental.
    Ready,
    /// A directory exists but has no branches: an interrupted clone.
    Interrupted,
    /// Nothing on disk.
    Absent,
}

impl MirrorState {
    fn probe(path: &Path) -> Self {
        if !path.exists() {
            Self::Absent
        } else if super::is_bootstrapped(path) {
            Self::Ready
        } else {
            Self::Interrupted
        }
    }
}

/// How consent for a needed bootstrap is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsentMode {
    /// `--noconfirm`: announce (so logs show what automation agreed to) and
    /// proceed.
    AutoYes,
    /// Ask via [`ui::confirm`] — dialoguer on a TTY; on a pipe the read-line
    /// fallback, where EOF takes the yes default and a piped `n` declines
    /// (`-Sy` in a script is itself the explicit ask, and cron/CI runs with a
    /// closed stdin keep working).
    Prompt,
    /// Implicit trigger with no terminal: never bootstrap on the user's behalf.
    Refuse,
}

/// Pure consent-resolution core, kept parameter-injected for the unit tests
/// ([`plan`] feeds it the live `--noconfirm` flag and stdin's TTY-ness).
const fn consent_mode(reason: RefreshReason, noconfirm: bool, stdin_is_tty: bool) -> ConsentMode {
    if noconfirm {
        ConsentMode::AutoYes
    } else if reason.is_explicit() || stdin_is_tty {
        ConsentMode::Prompt
    } else {
        ConsentMode::Refuse
    }
}

/// Pure decision core: what the AUR half wants to do, before consent.
const fn decide(aur_enabled: bool, state: MirrorState, reason: RefreshReason) -> AurAction {
    if !aur_enabled {
        return AurAction::Skip(SkipCause::Disabled);
    }
    match (reason, state) {
        (RefreshReason::ForceReclone, MirrorState::Ready) => {
            AurAction::Bootstrap(BootstrapKind::ForcedReclone)
        }
        (_, MirrorState::Ready) => AurAction::Fetch,
        (_, MirrorState::Interrupted) => AurAction::Bootstrap(BootstrapKind::InterruptedRedo),
        (_, MirrorState::Absent) => AurAction::Bootstrap(BootstrapKind::FirstRun),
    }
}

/// Resolve what this refresh does to the AUR mirror: the pure [`decide`] step,
/// then — when a bootstrap is wanted — the cost announcement and the consent
/// prompt. Must run before the progress display exists (a prompt under live
/// indicatif rows gets clobbered by redraws).
pub(super) fn plan(cfg: &Config, reason: RefreshReason) -> Result<AurAction> {
    let state = MirrorState::probe(&paths::aur_repo_path());
    let mut action = decide(cfg.aur, state, reason);
    match action {
        AurAction::Bootstrap(kind) => {
            action =
                match consent_mode(reason, runopts::noconfirm(), std::io::stdin().is_terminal()) {
                    ConsentMode::AutoYes => {
                        announce(kind);
                        AurAction::Bootstrap(kind)
                    }
                    ConsentMode::Prompt => {
                        announce(kind);
                        if ui::confirm(question(kind), false)? {
                            AurAction::Bootstrap(kind)
                        } else {
                            AurAction::Skip(SkipCause::Declined)
                        }
                    }
                    ConsentMode::Refuse => AurAction::Skip(SkipCause::NonInteractive),
                };
        }
        // Explicit CLI syncs get a one-line note; the shell words its own
        // outcome and the implicit resync surfaces the cause in its error.
        AurAction::Skip(SkipCause::Disabled)
            if matches!(
                reason,
                RefreshReason::ExplicitSync | RefreshReason::ForceReclone
            ) =>
        {
            ui::note(
                "AUR disabled (aur = false in config.toml); refreshing official package databases only",
            );
        }
        _ => {}
    }
    info!(reason = ?reason, state = ?state, action = ?action, "aur refresh plan");
    Ok(action)
}

/// Print the cost announcement for the flavour of clone about to be proposed.
fn announce(kind: BootstrapKind) {
    match kind {
        BootstrapKind::FirstRun => {
            ui::info("first-time AUR setup — aurox mirrors the whole AUR as one git repo");
            ui::note(
                "~2 GiB download, ~2.5 GiB on disk, ~8-9 min — one-time; refreshes afterwards are small incremental fetches",
            );
            ui::note("enables AUR search, info, install, and upgrades");
            ui::note(&format!(
                "pacman-only instead? set `aur = false` in {}",
                paths::config_path().display()
            ));
        }
        BootstrapKind::InterruptedRedo => {
            ui::warn("previous bootstrap was interrupted; the clone must restart from scratch");
            ui::note("~2 GiB download, ~8-9 min");
        }
        BootstrapKind::ForcedReclone => {
            ui::info("-Syy re-clones the AUR mirror from scratch: ~2 GiB download, ~8-9 min");
        }
    }
}

/// The Y/n question matching [`announce`]'s copy.
const fn question(kind: BootstrapKind) -> &'static str {
    match kind {
        BootstrapKind::FirstRun | BootstrapKind::InterruptedRedo => "clone the AUR mirror now?",
        BootstrapKind::ForcedReclone => "delete the existing mirror and re-clone?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_REASONS: [RefreshReason; 4] = [
        RefreshReason::ExplicitSync,
        RefreshReason::ForceReclone,
        RefreshReason::Shell,
        RefreshReason::IndexResync,
    ];

    /// `--noconfirm` is the automation opt-in: it approves a bootstrap for any
    /// trigger, terminal or not.
    #[test]
    fn noconfirm_auto_approves_every_reason() {
        for reason in ALL_REASONS {
            for tty in [true, false] {
                assert_eq!(
                    consent_mode(reason, true, tty),
                    ConsentMode::AutoYes,
                    "{reason:?} tty={tty}"
                );
            }
        }
    }

    /// An explicit command carries the intent even on a pipe — the prompt's
    /// read-line fallback (EOF ⇒ yes, piped `n` ⇒ decline) still applies, so
    /// scripts keep both levers.
    #[test]
    fn explicit_reasons_prompt_on_and_off_tty() {
        for reason in [
            RefreshReason::ExplicitSync,
            RefreshReason::ForceReclone,
            RefreshReason::Shell,
        ] {
            for tty in [true, false] {
                assert_eq!(
                    consent_mode(reason, false, tty),
                    ConsentMode::Prompt,
                    "{reason:?} tty={tty}"
                );
            }
        }
    }

    /// The implicit schema-bump resync may ask a present human but must never
    /// bootstrap behind a pipe.
    #[test]
    fn implicit_resync_prompts_only_on_a_tty() {
        assert_eq!(
            consent_mode(RefreshReason::IndexResync, false, true),
            ConsentMode::Prompt
        );
        assert_eq!(
            consent_mode(RefreshReason::IndexResync, false, false),
            ConsentMode::Refuse
        );
    }

    /// `aur = false` beats every trigger and every mirror state.
    #[test]
    fn disabled_config_skips_everything() {
        for state in [
            MirrorState::Ready,
            MirrorState::Interrupted,
            MirrorState::Absent,
        ] {
            for reason in ALL_REASONS {
                assert_eq!(
                    decide(false, state, reason),
                    AurAction::Skip(SkipCause::Disabled),
                    "{state:?} {reason:?}"
                );
            }
        }
    }

    /// A healthy mirror fetches incrementally — no consent involved — except
    /// under `-Syy`, which deliberately re-clones.
    #[test]
    fn ready_mirror_fetches_unless_force_recloned() {
        for reason in [
            RefreshReason::ExplicitSync,
            RefreshReason::Shell,
            RefreshReason::IndexResync,
        ] {
            assert_eq!(
                decide(true, MirrorState::Ready, reason),
                AurAction::Fetch,
                "{reason:?}"
            );
        }
        assert_eq!(
            decide(true, MirrorState::Ready, RefreshReason::ForceReclone),
            AurAction::Bootstrap(BootstrapKind::ForcedReclone)
        );
    }

    /// Missing or interrupted mirrors want a bootstrap whatever the trigger;
    /// the kind picks the announcement copy.
    #[test]
    fn missing_and_interrupted_mirrors_want_bootstrap() {
        for reason in ALL_REASONS {
            assert_eq!(
                decide(true, MirrorState::Absent, reason),
                AurAction::Bootstrap(BootstrapKind::FirstRun),
                "{reason:?}"
            );
            assert_eq!(
                decide(true, MirrorState::Interrupted, reason),
                AurAction::Bootstrap(BootstrapKind::InterruptedRedo),
                "{reason:?}"
            );
        }
    }

    /// The cause reads sensibly inside "AUR refresh skipped ({cause})".
    #[test]
    fn skip_cause_wording() {
        assert_eq!(SkipCause::Disabled.to_string(), "aur = false in config");
        assert_eq!(SkipCause::Declined.to_string(), "declined");
        assert_eq!(SkipCause::NonInteractive.to_string(), "non-interactive run");
    }
}
