#!/usr/bin/env bash
# The mixed repo+AUR upgrade demo driver, run as a plain test so the recorded
# flow can't rot. Seeds outdated installs via the same script the demo
# pipeline uses (demos/seed-upgrade.sh — one seed, two consumers), then
# asserts both packages actually moved. See docs/plans/screencasts.md.
source /work/tests/container/lib.sh
bootstrap; reset_state

source /work/demos/seed-upgrade.sh
assert_pkg_installed loop-repo
assert_pkg_installed test-hello

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_upgrade"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "upgrade demo driver failed (stage / review / apply)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_UPGRADE_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

pacman -Qi loop-repo | grep -q 'Version *: *2.0-1' || {
    echo "upgrade did not move loop-repo to 2.0" >&2
    pacman -Qi loop-repo | grep Version >&2; exit 1
}
pacman -Qi test-hello | grep -q 'Version *: *1.4-1' || {
    echo "upgrade did not move test-hello to 1.4" >&2
    pacman -Qi test-hello | grep Version >&2; exit 1
}
