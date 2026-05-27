# Optimizing the AUR mirror fetch

A record of the work that brought `gitaur -Sy` against the AUR mirror from
~11 s to ~3.5–5 s (warm cache). Most of the time was, and to some extent
still is, spent inside `gix` doing per-ref work on a ref store with ~155k
branches (one per AUR package).

This is the *what and why* of each change. For *how to capture a profile*
see [`PROFILING.md`](PROFILING.md); for reading a captured trace from the
terminal see the `gitaur-trace` section of the top-level `README.md`.

## Where the time goes

`gitaur` fetches with a refspec that maps **every** remote branch, so the
ref store has ~155k entries and several fetch phases are inherently
O(155k). The dominant gix spans of a warm incremental fetch, and what each
does:

| span | what it does | per-ref work |
| --- | --- | --- |
| `list refs` / `ref_map` | parse the server's ls-refs advertisement | builds the ~155k mappings |
| `mark mappings` | negotiate have-set: resolve each local ref, look up each want in the commit-graph | find + commit-graph lookup |
| `mark_all_refs` | seed the negotiation graph from all local refs | per-ref graph insert |
| `receiving pack` / `write_to_directory` | receive and index the pack | — |
| `git` (write_commit_graph) | `gitaur` shells out to system git to write a commit-graph after each fetch | — |
| `update_refs()` | turn each mapping into a ref edit, then `apply` the transaction | find + exists + edit-build |
| `apply` | commit the ref transaction (verifies every edit) | per-edit verify |

The recurring theme: **the same ~155k local refs get resolved in three
separate passes** (`mark mappings`, `update_refs`, and `apply`), and the
naive resolution path does a wasted loose-ref `open()` syscall, a binary
search that re-parses lines, and/or an owned allocation per ref.

## Measuring

Each run writes a Chrome/Perfetto trace to `state_dir()/traces/`. Inspect
it without a browser:

```sh
gitaur-trace                          # spans by total self time
gitaur-trace tree --span "update_refs()"
gitaur-trace tree --span "mark mappings"
```

The optimization-relevant spans carry split-phase timing fields, e.g.
`update_refs()` reports `{find_ms, exists_ms, ff_ms}` and `mark mappings`
reports `{find_ms, commit_ms}`. A low `exists_ms` is the signal that the
`update_refs` unchanged-ref fast path is engaging.

> Numbers below are warm steady-state on the AUR mirror; absolute values
> swing with system load (network `before first byte`, commit-graph write),
> so treat them as ratios, not benchmarks.

## The gix fork

All gix-side changes live on
[`nikicat/gitoxide#b-plus-c-integration`](https://github.com/nikicat/gitoxide/tree/b-plus-c-integration).
`gitaur`'s `Cargo.toml` pins a specific rev and documents each change in the
dependency comment. To iterate locally, add a patch pointing at a checkout:

```toml
[patch."https://github.com/nikicat/gitoxide"]
gix = { path = "/path/to/gitoxide/gix" }
```

Workflow for a gix change: instrument the hot loop with split-phase timing
→ measure on the mirror → implement → re-run the gix test matrix
(`cargo nextest run -p gix --features blocking-network-client fetch`) → push
the fork → re-pin `gitaur` → rebuild → verify the trace.

## Done

In rough chronological order. Fork commits are on `b-plus-c-integration`.

### 1. Skip name validation in the packed-refs binary search
`gix-ref` · upstream PR #2604 · `518173304`

The packed-refs binary search re-validated each candidate ref name
(`gix_validate::tag::name_inner`) on every comparator step — ~17× per
lookup. On a 154k-ref store this validation was the single largest CPU sink.
Skipped it in the binary-search comparator (the names in packed-refs were
already validated when written).

### 2. Lazy name index in `packed::Buffer`
`gix-ref` · `046d0ca0e`

A single ls-refs fetch does ~155k packed lookups against one buffer. After a
threshold (8 lookups) the buffer eagerly builds a `name → offset` HashMap and
serves all further lookups as one O(1) probe + a single decode at the offset,
instead of a `log₂(n)` binary search each. Single-shot CLI lookups pay
nothing.

### 3. Cache packed-refs in a HashMap during `update_refs`
`gix` · upstream PR #2605

Companion to #2. Together #1–#3 brought `gitaur -Sy` from ~11 s to ~5 s
(roughly `git fetch` parity at the time).

### 4. Skip the loose-ref probe when building the have-set
`gix-protocol` · `14a8a16f5`

`mark_complete_and_common_ref` resolved each local tracking ref with the full
ref lookup, which probes the loose ref file **before** packed-refs — a wasted
`open()` syscall per ref on a freshly-packed mirror. Snapshot packed-refs and
the set of loose ref names once; resolve packed-only names straight from the
snapshot via `try_find_packed_only`, falling back to the full lookup for names
that really are loose (preserving loose-over-packed precedence).
`find_ms` ~2.1 s → ~0.6 s.

### 5. `gitaur`: write a commit-graph after each fetch
`gitaur` · `a3788fd`

Shells out to `git commit-graph write --split` after a fetch. This is a cost in
its own right (the `git` span) but it's the enabler for #6: with a commit-graph
present, commit metadata lookups become an mmap binary search instead of an ODB
inflate. See #10 for how the incremental path feeds it.

### 6. Resolve refs via the commit-graph in `mark_all_refs`
`gix-protocol` · `c1386c33e`

`mark_all_refs_in_repo` inflated each ref's target object just to check it
wasn't a tag before inserting it into the negotiation graph — ~155k object
inflations. Resolve through the commit-graph first
(`get_or_insert_commit`); only annotated tags / symrefs (a handful) still
need to peel. `mark_all_refs` ~3.3 s → ~0.3 s (`peeled` drops from 155k to 1).

### 7. Packed-refs fast path in `update_refs`
`gix` · `8d1a5c24b`

The same wasted loose `open()` as #4, but in the *other* pass:
`update::update()` re-resolved every mapping's local tracking ref with
`try_find_reference` (loose-first). Applied the same packed-snapshot fast
path. `update_refs()` `find_ms` ~1.9 s → ~0.4 s.

### 8. Borrowed packed lookup in negotiate + `update_refs`
`gix-protocol` + `gix` · `a93e672b7`, part of `3a62d800c`

Both passes only need each ref's *target oid*, but `try_find_packed_only`
allocates an owned `Reference` (name `BString` + target) per ref. Use the
borrowed `packed::Buffer::try_find` and read `target()` directly (packed refs
are always direct). Drops 155k allocations per pass; `mark mappings`
`find_ms` ~−120 ms.

### 9. Fast-path unchanged direct refs in `update_refs`
`gix` · part of `3a62d800c`

On an incremental fetch ~99.97% of mappings are unchanged, yet each ran the
full path: a `repo.objects.exists()` probe of the remote id, the ref lookup,
a peel, and a fast-forward check. For an unchanged, direct, packed ref the
result is a guaranteed `NoChangeNeeded` whose object is present by definition
(it's our current target). Emit the **byte-identical** no-op edit directly,
skipping the exists probe, peel, and ff-walk. `update_refs()` self
~1.17 s → ~0.61 s (`exists_ms` ~430 ms → ~75 ms).

### 10. Feed the commit-graph the fetched tips instead of `--reachable`
`gitaur` · `git.rs` / `mirror.rs`

The post-fetch `git commit-graph write` ran `--reachable`, which re-walks all
~155k refs every time just to discover the handful of new commits. The
incremental fetch already knows them — they're the `new_oid`s in the
`RefUpdate` vector. The incremental path now pipes those tips via
`--stdin-commits`, so git graphs only their not-yet-graphed closure; the
bootstrap and full-rebuild paths keep `--reachable` (no delta to feed). The
`write_commit_graph` span dropped ~1.30 s → ~0.10 s on a 137-ref refresh.

### 11. Load the index concurrently with the fetch
`gitaur` · `mirror.rs`

`cmd_refresh` ran strictly serially: fetch, *then* load `index.bin`, apply,
save. But the load is local file I/O against a different file than anything the
network fetch touches, so it now runs on a scoped thread alongside
`incremental_fetch` (scoped so the loader borrows `&idx_path` — no `Arc`). The
~0.5–0.7 s load disappears entirely under the multi-second fetch; in the trace
the `load` span sits on its own thread track starting at t≈0, overlapping the
whole fetch. The post-fetch tail is now just `incremental_update` + `save` +
`write_commit_graph` (~0.35 s total).

### Tooling (not perf, but part of the arc)
- `gix-transport` http spans with curl CURLINFO timing (`90a0a85d3`),
  the `mark mappings` split-phase span (`d5b3ee00e`), and a `gix-ref`
  profiling helper (`fb64ef178`) — make the costs visible.
- `gitaur`: per-run OTEL→Chrome span traces (`f522c47`) and the
  `gitaur-trace` analysis CLI (`b2020e6`).
- `nextest` default-filter excluding two git-version-coupled `fetch_pack`
  tests so fork builds stay green on Arch's newer git (`f36a03f16`).

## Dead ends

### `apply`'s no-op edits are semantically locked
~800 ms, `edits = 155166`.

`update::update()` pushes a no-op `RefEdit` with
`PreviousValue::MustExistAndMatch` for **every** unchanged ref, and the
transaction re-verifies each one — a third full per-ref pass. Skipping the
no-op edits looks tempting but is **deliberate gix behavior**:
`update_refs/tests.rs::various_valid_updates` asserts that an identical-id
`NoChangeNeeded` mapping produces `edits.len() == 1`, and the
`MustExistAndMatch` check is the transaction's optimistic-concurrency guard.
Removing it diverges from documented semantics and breaks the upstream test.
Don't pursue without a real semantics discussion / upstreaming.

## Possible future work

Ordered roughly by expected payoff. None attempted yet.

- **`apply` (~800 ms) — batch/packed-aware verification.** The per-edit
  `MustExistAndMatch` verification re-resolves each ref a third time inside
  the `gix-ref` transaction layer. Rather than skipping the edits (the dead
  end above), make the transaction verify a large batch of no-op edits
  against the packed-refs snapshot in one pass. This is a deeper `gix-ref`
  change but keeps semantics intact.

- **`receive` side.** `write_to_directory` (pack index, ~1.3 s) is now a
  meaningful share of the total — the commit-graph write (#10) and the index
  load (#11) are handled. The pack index lives inside gix's `receive`, so
  overlapping it from gitaur isn't possible without restructuring; the
  remaining gitaur-side tail (`save`, ~0.25 s) is small enough that hiding it
  under the now-cheap commit-graph write would save little.

- **`mark mappings` `commit_ms` (~355 ms).** The `get_or_insert_commit`
  cutoff-date lookup per want. It's only *used* when at least one mapping
  changed — and on a real refresh something usually has — so a blanket "skip
  when nothing changed" rarely fires. A cheaper cutoff estimate might help.

- **`mark mappings` / `update_refs` `find_ms` residual (~300–500 ms).** After
  the fast paths this is the HashMap probe + per-line `decode` + the
  `loose_names.contains()` guard (hashing each of 155k names against a small
  set). Largely irreducible without restructuring the lookup into a single
  merge-join over the sorted packed-refs buffer and sorted mappings.

## Related

- [`PROFILING.md`](PROFILING.md) — capturing a samply CPU profile.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — why the mirror + zero-copy index
  design looks the way it does.
- Memory: `project_fetch_ref_resolution_passes`, `project_libgit2_http_slow`,
  `project_otel_chrome_tracing` (in the assistant's persistent notes).
