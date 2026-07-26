#!/usr/bin/env bash
# `-G` hands a pkgbase's repo to the user: cloned out of the local mirror (no
# network) into ./<pkgbase>, with `origin` rewritten to the AUR's *pushable*
# SSH endpoint — not the mirror we cloned from, and not the fetch-only HTTPS
# URL yay/paru leave behind. `-Gp` prints the PKGBUILD instead.
source /work/tests/container/lib.sh
bootstrap; reset_state
aurox -Sy

# Never clone into /work — that's the mounted source tree.
cd "$(mktemp -d)"

aurox -G test-trivial
assert_exit 0
[[ -f test-trivial/PKGBUILD ]] || { echo "expected ./test-trivial/PKGBUILD" >&2; ls -la >&2; exit 1; }
origin="$(git -C test-trivial remote get-url origin)"
[[ "$origin" == "ssh://aur@aur.archlinux.org/test-trivial.git" ]] \
    || { echo "origin is '$origin', expected the AUR SSH URL" >&2; exit 1; }
# A clone, not a file copy: the history came along, so a fix can be committed
# and pushed straight back.
git -C test-trivial log --oneline >/dev/null

# Re-running is not an overwrite — the user may have work in that directory.
aurox -G test-trivial
assert_exit 1
assert_stderr_contains "already exists"

aurox -Gp test-trivial
assert_exit 0
assert_stdout_contains "pkgname=test-trivial"

aurox -G no-such-pkgbase
assert_exit 1
assert_stderr_contains "not in the AUR: no-such-pkgbase"
