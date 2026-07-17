#!/usr/bin/env bash
# The repo-package install demo driver (`aurox -S repo-hello` typed in a demo
# bash — the pacman-parity fast path), run as a plain test so the recorded
# flow can't rot. See docs/plans/screencasts.md.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_repo_install"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "repo-install demo driver failed (sudo gate / passthrough)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_REPO_INSTALL_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

assert_pkg_installed repo-hello
