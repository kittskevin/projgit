# Design: Fetcher Strategy

> Status: **current as of 2026-05-11**. URL-backed mounts use
> `GitCliFetcher` as the default production path. `GixFetcher` remains
> available as an experimental/native-Rust transport behind the `gix-fetcher`
> feature. `GvfsFetcher` is available behind the `gvfs-fetcher` feature for
> remotes that expose GVFS protocol endpoints.

## Current Default

For URL sources, `projgit mount` creates or reuses a partial clone with the
system `git` executable:

```sh
git clone --filter=blob:none --no-checkout <url> <cache-dir>
```

At runtime, missing objects are hydrated by `GitCliFetcher`, which keeps one
long-lived child process alive:

```sh
git -C <git-dir> cat-file --batch-check
```

Reads and T1 header prefetch send object IDs to that child. In a partial clone,
stock Git automatically treats missing objects as promisor-remote fetches and
uses the protocol framing configured by the clone.

## Optional GVFS Backend

`GvfsFetcher` is an optional backend for remotes that support the GVFS v1 HTTP
protocol. It is not selected automatically today; callers must build with the
`gvfs-fetcher` feature and choose it explicitly:

```sh
cargo run -p projgit-cli --features gvfs-fetcher -- \
    mount <git-url> <mountpoint> --fetcher gvfs --gvfs-url <gvfs-base-url>
```

If `PROJGIT_GVFS_TOKEN` is set, the CLI passes it as a bearer token. Otherwise
the first implementation sends no auth header. Git credential-helper integration
is the next auth step.

The repository CI builds, lints, and tests with `gvfs-fetcher` enabled so this
path does not rot, but the feature is intentionally not part of the default
feature set.

Implemented GVFS endpoints:

- `GET /gvfs/objects/{objectId}` downloads one compressed loose Git object.
    projgit writes it into the local object store and verifies it with
    `ObjectStore::contains` before reporting success.
- `POST /gvfs/sizes` returns batched uncompressed object sizes. T1 prefetch uses
    this as metadata-only header warming for blob/symlink OIDs; it does not fetch
    blob bytes.

Deferred GVFS endpoints:

- `GET /gvfs/config` for server config, client-version policy, and cache-server
    discovery.
- `POST /gvfs/objects` for packfile or GVFS loose-object batch ingestion.
- `GET /gvfs/prefetch` for timestamped non-blob pack warming.

This keeps projgit's identity intact: GVFS is an acceleration backend for the
same read-only projection filesystem, not a pivot into a native GVFS working
tree client.

## Why Not Default To `GixFetcher`?

`GixFetcher` is the native-Rust path. It asks the remote for a single object by
OID using gitoxide's transport stack. That is attractive, but it depends on
server policy for bare-OID wants such as `allow-tip-sha1-in-want` or
`allow-reachable-sha1-in-want`.

The Phase 0a spike showed this can work, but later testing found that GitHub
rejects the bare-OID path for many repositories. The observed failure mode is
that the server reports `RejectedSourceObjectNotFound`, or the receive appears
to finish while no useful pack lands. The object remains missing.

The same object request succeeds when framed through Git's partial-clone
promisor machinery. For the project demo and default CLI behavior, reliability
against common hosted Git servers matters more than keeping the transport pure
Rust.

## Why Keep `GixFetcher`?

`GixFetcher` is still useful:

- It keeps a native-Rust transport path in the codebase for future work.
- It may work well with servers that allow reachable bare-OID wants.
- It is useful for benchmarks and experiments.
- It gives consumers a path that does not require a system `git` executable,
  once server behavior and protocol framing are better understood.

For those reasons, `projgit-core` keeps `gix-fetcher` default-on. Crates that do
not need it, including `projgit-cli`, `projgit-fuse`, and `projgit-winfsp`, use
`default-features = false` when depending on `projgit-core`.

## Future Promotion Criteria

`GixFetcher` can become the default again if it can perform the same promisor
fetch Git performs today, or if hosted server policy makes the bare-OID path
reliable enough for public demos.

Before changing the default, verify:

1. URL mounts hydrate missing blobs from GitHub reliably.
2. T1 header prefetch can batch without one subprocess per object.
3. Credential behavior is at least as ergonomic as system Git.
4. The fallback path remains clear for users without the needed server support.

## Future GVFS Criteria

Before making GVFS auto-detected or preferred for any URL class, verify:

1. The GVFS base URL can be derived or discovered reliably for real servers.
2. Auth works through Git credential helpers, not only bearer-token env vars.
3. Unsupported-server fallback is quiet, but auth/server failures are visible.
4. Packfile ingestion is implemented for efficient batched blob hydration.
5. A real GVFS-capable remote passes network-gated tests.