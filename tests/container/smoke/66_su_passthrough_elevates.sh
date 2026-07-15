#!/usr/bin/env bash
# `aurox -S{u,yu,yyu}` must forward the *exact* op cluster to pacman — `-Su`
# stays `-Su` (no implicit sync-DB refresh), `-Syyu` keeps its force-refresh
# double-y — and every form must offer sudo elevation. Both halves have
# regressed before: an early dispatch hardcoded `pacman -Syu` whenever `u`
# was set, and `needs_sudo` was an exact-string allowlist that missed `-Su`,
# running pacman unprivileged into "you cannot perform this operation unless
# you are root".
#
# We assert on the elevation preview (`:: about to elevate via sudo:` plus
# the command line) and answer "n" so nothing runs — the printed argv is the
# same vector `exec_pacman` would spawn.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox_input "n" -Su
assert_exit 1
assert_stderr_contains "about to elevate via sudo"
assert_stderr_contains "sudo pacman -Su"
# The historical rewrite: -Su must not grow a sync-DB refresh.
assert_stderr_not_contains "pacman -Sy"

aurox_input "n" -Syu
assert_exit 1
assert_stderr_contains "sudo pacman -Syu"

# A collapse to -Syu would fail this contains check ("-Syu" is not a
# substring of "-Syyu").
aurox_input "n" -Syyu
assert_exit 1
assert_stderr_contains "sudo pacman -Syyu"
