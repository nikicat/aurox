#!/usr/bin/env bash
# End-to-end of the STAGED repo-upgrade lane: apply stages the rootless
# store's frozen sync dbs into the system DBPath (elevated `install`) and runs
# a plain `pacman -Su` — never `--dbpath` (the 2026-07-25 corruption vector:
# pacman's commit path unlinks the store's `local` symlink mid-transaction).
#
# Scenario (the smoke/55 gap, driven through to an install):
#   * loop-repo 2.0 (the baked local-repo version) is installed; the system
#     sync db knows nothing newer, so `pacman -Qu` is silent.
#   * loop-repo 3.0 is published into the local repo AFTER the system db was
#     last synced — only the rootless refresh can see it. A landed 3.0 is
#     therefore proof the frozen db was staged: an unstaged `-Su` could at
#     best reach 2.0.
#   * The `shell_upgrade_staged_e2e` driver runs upgrade→apply under a PTY
#     and pins the one-consent two-command gate (and the absence of --dbpath).
#
# The `-Syu` fallback lane (store never populated) stays pinned by 04.
source /work/tests/container/lib.sh
bootstrap
reset_state

# Turn the rootless repo sync on (suite default is off) and strip the network
# repos so refresh and upgrade are hermetic — only the file:// local-repo.
cat > "$CONFIG_DIR/config.toml" <<EOF
mirror_url = "file://$MOCK_AUR"
check_repo_updates = true
EOF
awk '/^\[/ { keep = ($0 == "[options]" || $0 == "[local-repo]") } keep' \
    /etc/pacman.conf > /tmp/pacman.conf.hermetic
sudo cp /tmp/pacman.conf.hermetic /etc/pacman.conf

# Install the baked 2.0 from the local repo.
sudo pacman -S --noconfirm loop-repo >/dev/null
assert_pkg_installed loop-repo
pacman -Qi loop-repo | grep -q 'Version *: *2.0-1' || {
    echo "seed install is not 2.0" >&2; pacman -Qi loop-repo | grep Version >&2; exit 1
}

# Publish a real 3.0 into the local repo (built from the fixture PKGBUILD, as
# extended/04 builds its 1.0 seed) — the system db was synced before this, so
# only the rootless refresh can see it.
work="$(mktemp -d)"
cp /work/tests/container/fixtures/loop-repo/PKGBUILD "$work/"
sed -i 's/^pkgver=.*/pkgver=3.0/' "$work/PKGBUILD"
( cd "$work" && makepkg --noconfirm --nodeps --skipinteg )
cp "$work"/loop-repo-3.0-*.pkg.tar.zst "$LOCAL_REPO/"
repo-add --quiet "$LOCAL_REPO/local-repo.db.tar.gz" \
    "$LOCAL_REPO"/loop-repo-3.0-*.pkg.tar.zst >/dev/null

# Precondition: the SYSTEM db must not see 3.0 — otherwise a plain unstaged
# `-Su` would land it too and the test would prove nothing.
if pacman -Qu 2>/dev/null | grep -q '^loop-repo '; then
    echo "precondition failed: system db already shows the loop-repo upgrade" >&2
    exit 1
fi

# Drive the staged upgrade flow under a PTY.
driver="$EXAMPLES_DIR/shell_upgrade_staged_e2e"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "staged shell upgrade driver failed" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'SHELL_UPGRADE_STAGED_E2E_OK' "$out" || {
    echo "driver did not report success" >&2; cat "$out" >&2; exit 1
}

# The upgrade landed the version only the frozen store knew about.
pacman -Qi loop-repo | grep -q 'Version *: *3.0-1' || {
    echo "staged upgrade did not move loop-repo to 3.0" >&2
    pacman -Qi loop-repo | grep Version >&2
    cat "$out" >&2
    exit 1
}

# The incident's smoking gun stays impossible: the store's `local` is still a
# symlink after the apply (a pre-fix `--dbpath` run turned it into a real
# root-owned dir mid-transaction).
[[ -L "$STATE_DIR/syncdb/local" ]] || {
    echo "store local is no longer a symlink after apply:" >&2
    ls -la "$STATE_DIR/syncdb/" >&2
    exit 1
}

# Staging really wrote the system DBPath: byte-equal frozen and system dbs.
system_db="$(pacman-conf DBPath)"
cmp "$STATE_DIR/syncdb/sync/local-repo.db" "$system_db/sync/local-repo.db" || {
    echo "system sync db differs from the frozen store db after staging" >&2
    ls -la "$STATE_DIR/syncdb/sync/" "$system_db/sync/" >&2
    exit 1
}

echo "OK — staged apply: frozen db staged into the system DBPath, plain -Su landed 3.0, store symlink intact"
