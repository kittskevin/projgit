# Phase 0a Spike — Results

> **Outcome: Branch A (gix on-demand single-blob fetch WORKS)**.
> The single biggest project risk is retired. `projgit-core`'s `Fetcher`
> can be built on `gix` directly; no `git` CLI subprocess is required for
> the basic on-demand path.

## Spike question

> Can gitoxide (`gix`) fetch a single, specific blob OID from a promisor
> remote on demand, given a partial clone created with
> `git clone --filter=blob:none --no-checkout`?

(See [docs/initial-plan.md](../../docs/initial-plan.md) §5.4 and the
"Why 0a matters" discussion in chat history for context.)

## Setup

- **Toolchain:** stable rustc/cargo 1.95.0 (updated from 1.75 during the
  spike — gix's modern dep tree requires `edition2024` cargo support).
- **`gix` version:** 0.66.0.
- **Features:** `blocking-network-client` +
  `blocking-http-transport-reqwest-rust-tls` + `max-performance-safe`.
- **Test repo:** small public repo
  `https://github.com/rust-lang/log` (~500 KB blobless clone).
- **OS:** Windows 11 (cmd / PowerShell host).
- **Network:** Residential broadband.

## Method

```text
git clone --filter=blob:none --no-checkout \
    https://github.com/rust-lang/log spikes/ondemand-fetch/testrepo
cd spikes/ondemand-fetch && cargo run --release
```

The spike (`src/main.rs`):

1. Opens the partial clone with `gix::open`.
2. Resolves `HEAD` → commit → tree.
3. Walks the top-level tree to pick the first blob entry.
4. Confirms the blob is **not** locally present
   (`repo.try_find_object(oid)?.is_none()`).
5. Builds a single-refspec fetch `+<oid>:refs/projgit-spike/wanted`,
   connects to `origin`, and runs the fetch lifecycle:
   `Remote::connect → prepare_fetch → receive`.
6. Re-checks presence and dumps blob size on success.

## Headline output

```
== Phase 0a spike: gitoxide on-demand fetch ==
Repo path: testrepo
[25.971ms] Opened repo. git_dir = testrepo\.git
HEAD commit: 67bc7e32c68a4a8908d1016693418f12b43bab90
HEAD tree:   62576897045db324f6ba64f398955e78220a3fc4
Test blob OID: 1503014591392cd9a99c6aaa0efe17e4eedacab2
Locally present BEFORE fetch: false

--- Attempting on-demand fetch via gix ---
  Using refspec: +1503014591392cd9a99c6aaa0efe17e4eedacab2:refs/projgit-spike/wanted
  Connecting to remote...
  Preparing fetch...
  Receiving pack...
  receive() returned Ok.
[430.8264ms] gix on-demand fetch returned Ok.
Locally present AFTER fetch:  true
Fetched blob size: 26 bytes

*** SPIKE RESULT: Branch A (gix on-demand fetch WORKS) ***
```

## Findings

### What worked

1. **Single-blob fetch via custom refspec.** Passing
   `+<oid>:refs/projgit-spike/wanted` as the only fetch refspec succeeds.
   GitHub honors `allow-tip-sha1-in-want` /
   `allow-reachable-sha1-in-want`, so any reachable OID can appear in
   the `want` line.
2. **Standard credentials path.** No auth was required for the public
   `rust-lang/log` repo. For private repos, gix's
   `gix-credentials` integrates with the system git credential helper
   chain; not stress-tested in this spike.
3. **HTTPS transport via `reqwest` + `rustls`.** The
   `blocking-http-transport-reqwest-rust-tls` feature bundle works on
   Windows out of the box. No system OpenSSL or libgit2 dependency.
4. **Pack lifecycle.** The remote returned a tiny pack containing only
   the requested object. `receive()` indexed it and made it visible to
   subsequent `try_find_object` calls without further intervention.
5. **`gix::interrupt::IS_INTERRUPTED`** is the standard interrupt flag;
   passing it integrates cleanly with future cancellation hooks.

### What did NOT work / caveats

1. **No transparent promisor auto-fetch.** Stock `git cat-file -p <oid>`
   on a partial clone *automatically* triggers a promisor fetch when the
   object is missing. `gix::Repository::try_find_object` does **not** do
   this — it returns `Ok(None)` for missing objects. The Fetcher must be
   called explicitly. (This is the architecturally cleaner outcome
   anyway: explicit hydration triggers are easier to instrument and
   coalesce.)
2. **`replace_refspecs` mutates remote state for the connection only**;
   it does not persist to the on-disk config. Fine for our use; worth
   noting so we don't expect side effects.
3. **Cold-fetch latency floor: ~430 ms.** Includes DNS, TLS handshake,
   ref-advertisement (protocol v2), `want`-line negotiation, packfile
   download (small), and pack indexing. On a fast link this is likely
   ~250–350 ms in steady state; the first fetch in a process is the
   slow one. **Implication:** every cache miss on a cold connection
   costs ~half a second. Connection reuse and bulk-prefetch heuristics
   in the real Fetcher will be important.

### Not yet measured (deferred to Phase 2)

- **Concurrent fetches.** Single-flight coalescing not exercised. We
  expect to need a `tokio::sync::Mutex<HashMap<Oid, Shared<Future>>>`
  per the plan, regardless of which Fetcher backend is in use.
- **Connection reuse / keep-alive across fetches.** A second fetch in
  the same process should be much faster than the first; not measured.
- **Auth flows** with credential helpers, SSH, hardware tokens.
- **Error surface.** What does a network drop, a bogus OID, or a
  rate-limit response look like in `Result<_, gix::remote::fetch::Error>`?
- **Pack proliferation.** Each on-demand fetch creates a new pack.
  Need to verify gix-odb's MIDX / repacking story for production use.

## Implications for the plan

1. **Lock Branch A.** `projgit-core`'s Fetcher is built on `gix` first.
   `GixFetcher` is the MVP implementation. `GitCliFetcher` becomes a
   future fallback for any environment / protocol where Branch A
   degrades.
2. **Update [docs/initial-plan.md](../../docs/initial-plan.md) §5.4** to
   reflect "Branch A confirmed" and remove the "if gitoxide on-demand
   support is incomplete" hedge.
3. **Add Phase 2 sub-tasks** for:
   - Connection reuse / pool inside `GixFetcher`.
   - Pack-proliferation mitigation (periodic auto-repack).
   - Auth path stress test (private GitHub repo, SSH).
4. **Cargo-feature decision:** the
   `blocking-http-transport-reqwest-rust-tls` bundle is a strong default
   for cross-platform builds (no system TLS dep). Adopt it in
   `projgit-core`.

## Reproducibility

- Spike crate: [Cargo.toml](Cargo.toml), [src/main.rs](src/main.rs).
- Test repo created with the command in **Setup**; not committed.
- Re-running the spike after a successful fetch will see the blob as
  already-present and skip the fetch attempt with a NOTE message. To
  reset, delete `spikes/ondemand-fetch/testrepo/` and re-clone.

## Date

Run on 2026-05-08.
