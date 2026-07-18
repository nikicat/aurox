#!/usr/bin/env bash
# A consented PKGBUILD review persists across sessions: the second run over
# the identical mirror commit skips the prompt with an "already reviewed"
# note (reviews.db, keyed by pkgbase + commit). The commit-scoping — a new
# AUR push re-prompts even at the same pkgver — is unit-tested in
# src/build/reviews.rs; this scenario covers the disk round-trip through two
# real aurox invocations.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

# First install: the review prompt renders (interactive path, no
# --noconfirm) and EOF defaults it to approve — a prompt-won decision with
# the PKGBUILD on screen, so it lands in reviews.db.
aurox_input "" -S test-trivial
assert_exit 0
assert_pkg_installed test-trivial
assert_stderr_contains "review — (y)es"
[ -f "$STATE_DIR/reviews.db" ] || {
    echo "reviews.db should exist after a prompt-won approval" >&2
    _dump >&2
    exit 1
}

# Wipe the build cache so the pipeline reaches the review gate again (a
# cached artifact short-circuits before review), then reinstall the same
# version in a fresh invocation: the stored approval covers this exact
# commit, so the note fires instead of the prompt.
rm -rf "$STATE_DIR/pkgs"
aurox_input "" -S test-trivial
assert_exit 0
assert_stderr_contains "already reviewed"
if grep -qF "review — (y)es" "$LAST_STDERR"; then
    echo "second install of the same commit must not re-prompt" >&2
    _dump >&2
    exit 1
fi
