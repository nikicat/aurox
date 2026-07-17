#!/usr/bin/env bash
# The one-shot CLI install demo driver (`aurox -S test-hello` typed in a demo
# bash), run as a plain test so the recorded flow can't rot. See
# docs/plans/screencasts.md and tests/container/extended/33 for the pattern.
source /work/tests/container/lib.sh
bootstrap; reset_state

aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_cli_install"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "cli-install demo driver failed (review / build / sudo gate)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_CLI_INSTALL_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

assert_pkg_installed test-hello
