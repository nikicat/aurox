#!/usr/bin/env bash
# The Ctrl-C-during-refresh demo driver, run as a plain test so the recorded
# flow can't rot. The mirror is bootstrapped from the fast file:// mock AUR;
# the driver then repoints it at a server that answers headers then hangs
# (examples/hung_mirror.rs), types `refresh`, sends a real ^C mid-fetch, and
# proves the shell survived by quitting cleanly (finish_clean asserts exit 0).
# The `DEMO_CTRLC_REFRESH_OK` sentinel is printed only if every expect —
# including "refresh: interrupted" and the clean quit — held. See
# docs/TODO.md "Shell" and docs/plans/screencasts.md.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Bootstrap the mirror + index from the real (fast) mock AUR; the driver
# switches the remote to the hung server afterwards.
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_ctrlc_refresh"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "ctrl-c-refresh demo driver failed (refresh / interrupt / quit)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_CTRLC_REFRESH_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

echo "OK — refresh interrupted, shell survived to a clean quit"
