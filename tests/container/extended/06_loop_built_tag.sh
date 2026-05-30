#!/usr/bin/env bash
# End-to-end of the upgrade loop's already-built column.
#
# When an AUR upgrade candidate's *new*-version artifact is already sitting in
# its build worktree — the state a build that completed in an earlier batch but
# wasn't yet installed leaves behind — the picker (and change-set preview) flag
# it `built`: a `pacman -U` would reuse the cached `.pkg.tar.zst` instead of
# rebuilding. The detection is a read-only mirror of `prepare_one`'s idempotency
# check (`build::artifacts_built`), so this test proves the real worktree path +
# artifact filename + index version all line up end to end.
#
# The loop is interactive (needs a TTY), so the assertion lives in the
# `loop_built_tag_e2e` example, which drives the real binary under a PTY and
# asserts the picker row carries the `built` tag. Here we stage the state:
# install an outdated foreign copy, publish a newer version to the mock AUR,
# and pre-place the newer artifact in the worktree.
source /work/tests/container/lib.sh
bootstrap; reset_state

PKGBASE=test-trivial

# 1. Seed an installed-but-outdated foreign copy at 1.0-1: build the fixture
#    as-is and `pacman -U` it. It's then in localdb, absent from every sync
#    repo, and its pkgbase is in the mock AUR — exactly the foreign-AUR-upgrade
#    shape `aur_upgrades` surfaces.
work="$(mktemp -d)"
cp /work/tests/container/fixtures/$PKGBASE/PKGBUILD "$work/"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
sudo pacman -U --noconfirm "$work"/$PKGBASE-1.0-1-*.pkg.tar.zst
assert_pkg_installed $PKGBASE

# 2. Publish 2.0-1 of the same pkgbase into the mock AUR so the loop sees an
#    upgrade. The mirror is a writable bare repo owned by `builder`; push a
#    fresh commit to the pkgbase branch.
bump="$(mktemp -d)"
# `--no-hardlinks`: the bare mirror and $TMPDIR sit on different mounts, so
# git's default local-clone hardlink optimization aborts ("hardlink different
# from source"); copy the objects instead.
git clone --quiet --no-hardlinks --branch "$PKGBASE" "$MOCK_AUR" "$bump"
( cd "$bump"
  sed -i 's/^pkgver=.*/pkgver=2.0/' PKGBUILD
  makepkg --printsrcinfo > .SRCINFO
  git -c user.email=t@t -c user.name=t commit -aqm "$PKGBASE: bump to 2.0"
  git push --quiet origin "$PKGBASE" )

# 3. Pre-place the 2.0-1 artifact in the build worktree — the leftover of a
#    prior build that wasn't yet installed. The loop's read-only built-check
#    globs exactly this dir (`paths::pkg_worktree(pkgbase)`).
wt="$STATE_DIR/pkgs/$PKGBASE"
mkdir -p "$wt"
v2="$(mktemp -d)"
cp "$bump/PKGBUILD" "$v2/"
( cd "$v2" && makepkg --noconfirm --nodeps --skipinteg )
cp "$v2"/$PKGBASE-2.0-1-*.pkg.tar.zst "$wt/"
ls "$wt"/$PKGBASE-2.0-1-*.pkg.tar.zst >/dev/null 2>&1 || {
    echo "failed to stage the 2.0-1 artifact under $wt" >&2
    ls -la "$wt" >&2 || true
    exit 1
}

# 4. Drive the loop under a PTY. The example refreshes the mirror (picking up
#    2.0), renders the picker, and asserts the candidate row carries `built`.
driver="/work/target/debug/examples/loop_built_tag_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    echo "loop driver failed (picker did not flag the candidate as 'built'?)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'BUILT_TAG_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

echo "OK — loop picker flagged the pre-built $PKGBASE 2.0 candidate as 'built'"
