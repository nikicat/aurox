# Profiling aurox refreshes

How to capture a CPU profile of a refresh. `scripts/profile-refresh.sh` runs
samply and prints a flat self/total time table, so you can see which gix
functions dominate a run whose time is mostly gix-internal work over the
mirror's ~155k refs.

## One-time setup

```sh
cargo install samply
sudo sh -c 'echo 1    > /proc/sys/kernel/perf_event_paranoid
             echo 2048 > /proc/sys/kernel/perf_event_mlock_kb'
```

Both sysctls reset on reboot. `perf_event_paranoid<=1` lets non-root
users open perf events; `perf_event_mlock_kb>=2048` raises the per-CPU
ring-buffer allowance samply needs.

## Run

```sh
scripts/profile-refresh.sh                       # profiles `aurox -Sy`
scripts/profile-refresh.sh -- -S some-package    # any args after --
scripts/profile-refresh.sh -o /tmp/p.json.gz     # custom output path
```

Outputs `profile.json.gz` + `profile.json.syms.json`. Open interactively
with `samply load profile.json.gz`.

## What to look for

Which phases cost what, why they're O(155k refs), and which are already
optimized away is [`FETCH_OPTIMIZATION.md`](FETCH_OPTIMIZATION.md)'s subject —
read its "Where the time goes" table before interpreting a profile, or you'll
re-discover a hotspot the gix fork already fixed. That doc also carries the
fork-patching workflow (`[patch."https://github.com/nikicat/gitoxide"]`, the
gix test matrix, re-pinning), the one known dead end, and the remaining
candidates.

What samply adds over the span trace is *sub-span* attribution: the trace says
`update_refs()` spent 400 ms in `find_ms`, the profile says which gix function
inside it burned the samples. Reach for it when a span's own fields stop
explaining the number.
