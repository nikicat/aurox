# shellcheck shell=bash
# Seed installed-but-outdated packages for the upgrade demo (sourced inside
# the container after lib.sh, by demos/build.sh's record step AND by
# tests/container/extended/36_demo_upgrade.sh — one seed, two consumers).
#
# The baked local-repo carries loop-repo 2.0 and the mock AUR test-hello 1.4;
# installing locally-built 1.0 versions of each gives `upgrade` a mixed
# repo + AUR change set. Same mechanics as extended/04's loop-repo seed.

seed_outdated() {
    local fixture="$1"
    local work
    work="$(mktemp -d)"
    # The whole fixture dir: test-hello's PKGBUILD references its .install.
    cp "/work/tests/container/fixtures/$fixture/"* "$work/"
    sed -i 's/^pkgver=.*/pkgver=1.0/' "$work/PKGBUILD"
    (cd "$work" && makepkg --noconfirm --nodeps --skipinteg >/dev/null 2>&1)
    sudo pacman -U --noconfirm "$work/$fixture"-1.0-*.pkg.tar.zst >/dev/null
}

seed_outdated loop-repo
seed_outdated test-hello

# The demo types a bare `upgrade` — whole system, like pacman -Syu. That is
# only hermetic with the real repos out of the picture: the image's core/extra
# DBs are synced at bake time while installed packages come from a possibly
# older cached layer, so genuine pending updates can exist and a bare upgrade
# would stage real multi-MiB downloads. Local-repo-only pacman.conf makes
# "the whole system" truthfully consist of the seeded fixtures.
sudo sed -i '/^\[core\]/,/^Include/d; /^\[extra\]/,/^Include/d' /etc/pacman.conf
