#!/usr/bin/env bash
# End-to-end of the no-arg `gaur` upgrade loop, including its UI rendering.
#
# The loop is interactive (needs a TTY), so the assertions live in the
# `upgrade_loop_e2e` example, which drives the real binary under a PTY and walks
# the rendered sequence: picker → change-set preview → sudo gate → "all selected
# upgrades applied". Here we just stage an installed-but-outdated repo package
# (build 1.0 locally; the image's local-repo already carries 2.0), run the
# driver, and confirm the upgrade actually landed.
source /work/tests/container/lib.sh
bootstrap
reset_state

# Seed an outdated install: build loop-repo 1.0 from the fixture (the baked
# local-repo has 2.0) and install it, so the loop has a pending repo upgrade.
work="$(mktemp -d)"
cp /work/tests/container/fixtures/loop-repo/PKGBUILD "$work/"
sed -i 's/^pkgver=.*/pkgver=1.0/' "$work/PKGBUILD"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
sudo pacman -U --noconfirm "$work"/loop-repo-1.0-*.pkg.tar.zst
assert_pkg_installed loop-repo
pacman -Qi loop-repo | grep -q 'Version *: *1.0-1' || {
    echo "seed install is not 1.0" >&2; pacman -Qi loop-repo | grep Version >&2; exit 1
}

# Drive the loop under a PTY. The example asserts the UI sequence rendered and
# prints LOOP_E2E_OK on a clean exit.
driver="/work/target/debug/examples/upgrade_loop_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    echo "loop driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'LOOP_E2E_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

# The upgrade the loop applied must have actually moved localdb to 2.0.
pacman -Qi loop-repo | grep -q 'Version *: *2.0-1' || {
    echo "loop did not upgrade loop-repo to 2.0" >&2
    pacman -Qi loop-repo | grep Version >&2
    cat "$out" >&2
    exit 1
}

echo "OK — upgrade loop drove picker → change-set → apply → done, loop-repo now 2.0"
