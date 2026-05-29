#!/usr/bin/env bash
# Regression for the change-set preview's size source.
#
# Bug: the upgrade-loop preview used to read repo download sizes from the
# *system* pacman syncdb. Gitaur never `-Sy`s the system db (it refreshes its
# own rootless one), so the system syncdb still holds the *installed* version,
# whose `.pkg.tar.zst` is sitting in the pacman cache from when it was
# installed — and libalpm's `download_size()` returns 0 for a cached file.
# The preview then rendered `0 B` for almost every repo upgrade.
#
# This test recreates that condition deliberately and asserts the preview
# shows a real nonzero total. The driver carries the assertion (any
# `total  0 B` in the change-set screen is treated as the bug); here we just
# stage the stale-system-db scenario it runs against.
#
# Scenario (lifted from smoke/55_rootless_repo_db_check.sh, then driven through
# the upgrade loop):
#   * Hermetic pacman.conf — only the file:// local-repo remains; gitaur's
#     rootless `-Sy` would otherwise hit network mirrors.
#   * loop-repo 1.0-1 is published to the local repo, the system db is `-Sy`'d
#     to that version, and 1.0-1 is installed via `pacman -S` (which caches
#     the archive in /var/cache/pacman/pkg).
#   * loop-repo 2.0-1 is republished into the local repo *without* a system
#     `-Sy`. The system db stays at 1.0-1, the rootless db gitaur will fetch
#     carries 2.0-1.
#   * The loop driver runs gaur — it refreshes its rootless db (sees 2.0),
#     shows the preview, and the driver asserts `total  0 B` is absent.
#     - Fixed code reads sizes from the synced db → loop-repo 2.0's archive
#       isn't cached → real nonzero download size.
#     - Buggy code reads from the system db → loop-repo 1.0's archive IS
#       cached → `download_size()` returns 0 → preview total `0 B` → assert
#       fails → regression caught.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Turn on the rootless official-repo sync (off by default in the suite to keep
# refreshes hermetic) and strip pacman.conf to the local-repo so the rootless
# `-Sy` only ever hits file://.
cat > "$CONFIG_DIR/config.toml" <<EOF
mirror_url = "file://$MOCK_AUR"
check_repo_updates = true
EOF
awk '/^\[/ { keep = ($0 == "[options]" || $0 == "[local-repo]") } keep' \
    /etc/pacman.conf > /tmp/pacman.conf.hermetic
sudo cp /tmp/pacman.conf.hermetic /etc/pacman.conf

# Build loop-repo 1.0-1 from the fixture; the baked image already carries the
# 2.0 build in /srv/local-repo.
work="$(mktemp -d)"
cp /work/tests/container/fixtures/loop-repo/PKGBUILD "$work/"
sed -i 's/^pkgver=.*/pkgver=1.0/' "$work/PKGBUILD"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
pkg_one="$(ls "$work"/loop-repo-1.0-*.pkg.tar.zst)"
pkg_two="$(ls "$LOCAL_REPO"/loop-repo-2.0-*.pkg.tar.zst)"
[[ -f "$pkg_one" ]] || { echo "failed to build loop-repo 1.0" >&2; exit 1; }
[[ -f "$pkg_two" ]] || { echo "baked loop-repo 2.0 missing" >&2; exit 1; }

# Publish 1.0 into the local repo and sync the SYSTEM db to it. `pacman -S`
# downloads from file:// into /var/cache/pacman/pkg, so the 1.0 archive ends
# up cached — the precondition for the buggy `download_size()` to return 0.
sudo cp "$pkg_one" "$LOCAL_REPO/"
sudo repo-add --quiet "$LOCAL_REPO/local-repo.db.tar.gz" "$LOCAL_REPO/$(basename "$pkg_one")" >/dev/null
sudo pacman -Sy >/dev/null
sudo pacman -S --noconfirm loop-repo >/dev/null
assert_pkg_installed loop-repo
pacman -Qi loop-repo | grep -q 'Version *: *1.0-1' || {
    echo "seed install is not 1.0-1" >&2; pacman -Qi loop-repo | grep Version >&2; exit 1
}
# Confirm the archive landed in the cache — without it the buggy code wouldn't
# return 0 and the test would prove nothing.
ls /var/cache/pacman/pkg/loop-repo-1.0-*.pkg.tar.zst >/dev/null 2>&1 || {
    echo "precondition failed: loop-repo 1.0 archive not cached" >&2
    ls /var/cache/pacman/pkg/ >&2
    exit 1
}

# Republish 2.0 into the local repo. `repo-add` rewrites the db entry for the
# pkgname to the newer version; the .pkg files for both stay on disk. We do
# NOT `pacman -Sy` here — the SYSTEM syncdb must stay at 1.0-1 so the buggy
# `system_pac()` path reads the cached old version.
sudo repo-add --quiet "$LOCAL_REPO/local-repo.db.tar.gz" "$pkg_two" >/dev/null
# `pacman -Sy` compares mtimes to decide whether the source db has changed. The
# two `repo-add`s above ran milliseconds apart from the previous `pacman -Sy`'s
# write of the sync-state file, so pacman would otherwise treat the source as
# unchanged and skip the refresh — leaving the system db at 1.0 forever and
# making the loop's `pacman -Syu` a no-op. Nudge the source mtime forward so
# the next `-Sy` (the one the loop's apply will run) is genuinely the newer one.
sudo touch -d "+1 minute" "$LOCAL_REPO/local-repo.db.tar.gz"

# Precondition: system `pacman -Qu` must still see no upgrade (its db is
# stale). If it did, gaur's `-Sy` wouldn't be the only one closing the gap and
# the test wouldn't prove the regression.
if pacman -Qu 2>/dev/null | grep -q '^loop-repo '; then
    echo "precondition failed: system db already shows the loop-repo upgrade" >&2
    pacman -Qu >&2
    exit 1
fi

# Drive the loop under a PTY. The example asserts the UI sequence rendered
# AND that the change-set total isn't `0 B` (the regression marker), then
# prints LOOP_E2E_OK on a clean exit.
driver="/work/target/debug/examples/upgrade_loop_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! GITAUR="$GITAUR" "$driver" >"$out" 2>&1; then
    # Single-quoted so bash doesn't try to command-substitute the literal
    # 'total  0 B' inside backticks.
    echo 'loop driver failed (likely the size-source regression — "total  0 B" in the preview)' >&2
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

echo "OK — preview size came from the synced db (nonzero total), loop-repo now 2.0"
