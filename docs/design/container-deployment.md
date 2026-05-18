# Design: Container Deployment Topology

> Status: **T1 verified end-to-end (including non-root container user)
> as of 2026-05-18**; T2 follows the same shape; T3 unverified pending
> the Phase C bench. Captures the deployment shapes projgit's workload
> pitch implicitly relies on, what's actually shipped today, what's
> blocked, and the empirical evidence behind each claim.
>
> Read this alongside [`workload.md`](workload.md) §1.6 (the
> "many short-lived processes per host" claim that motivates
> containerized deployment) and the
> [audit](/memories/repo/audit.md) findings A1 (no daemon) and A3
> (cross-process single-flight gap) which this doc deliberately does
> not solve, just frames.

## 0. Why this document exists

projgit's value pitch is "100 short-lived containers per host, all
projected from one shared CAS." That implicitly commits to a
deployment topology where many container processes consume one
projection — and depending on which exact topology you mean, the
constraints are very different. Without a single place that names the
topologies and says which ones work today, every container-deployment
question turns into a re-derivation.

This document names the three topologies, says what's blocking each,
and records the empirical evidence (commands run, outputs observed)
that backs the claim.

## 1. The three topologies

### T1: projgit on the host, N containers consume the mount

```
host:  projgit mount https://… /var/projgit/<hash> --allow-other
       (FUSE mount established in the host's mount namespace,
        owned by some `projgit-svc` UID)
       │
       ├── docker run -v /var/projgit/<hash>:/repo:ro,bind-propagation=rslave …
       ├── docker run -v /var/projgit/<hash>:/repo:ro,bind-propagation=rslave …
       └── … N times
```

One mount, one fetcher, one upstream-connection-per-OID story. Matches
the §1.6 sequential amortisation model the bench already validates.
**Status: verified end-to-end inside the devcontainer 2026-05-18.**

### T2: projgit in a sidecar container, sibling containers consume

```
container A (sidecar):  projgit mount, bind-published to a shared volume
containers B/C/D…:      bind-mount the shared volume read-only
```

Conceptually identical to T1 from projgit's perspective, but the
projgit process is itself containerized. Sidecar needs `--privileged`
or `--cap-add SYS_ADMIN --device /dev/fuse` plus the right bind
propagation to publish its mount back through the host. **Status: not
verified, but no projgit-side blocker beyond T1's.** The remaining
work is orchestration (Dockerfile + entrypoint + propagation flags).

### T3: one projgit per container, all sharing the on-disk CAS

```
container 1:  projgit mount … --cache-dir /shared/cache  (own mount inside)
container 2:  projgit mount … --cache-dir /shared/cache  (own mount inside)
container N:  …
```

Each container does its own `projgit mount`, all pointing `--cache-dir`
at a shared volume. Multiple projgit processes share storage. **Status:
unverified.** The §1.6 *sequential* amortisation we measured today
already covers the case where containers come up one at a time and each
one's first read of a given OID may hit a warm on-disk CAS from a
previous container. The unverified case is **concurrent** first-touches
of the same OID — the audit A3 cross-process single-flight gap, which
the [bench Phase C](../bench/baseline.md) follow-up is designed to put
a number on.

## 2. The blocker every topology hit

Default FUSE mounts are accessible only to the mounting UID. The
kernel-level check fires before the userspace `Filesystem` handler
sees the request, so no amount of cleverness in our adapter can work
around it. From the experiment log:

```
$ sudo -n ls /tmp/mp-exp
ls: cannot access '/tmp/mp-exp': Permission denied
$ sudo -n cat /tmp/mp-exp/Cargo.toml
cat: /tmp/mp-exp/Cargo.toml: Permission denied
```

Even `root` is denied. Bind-mounting into a different mount namespace
(simulated with `unshare --mount` — the exact mechanism Docker `-v`
uses) succeeds at the kernel level, but reads from inside the new
namespace fail with the same `Permission denied` because the FUSE
security check is namespace-agnostic.

**The fix:** pass `-o allow_other` to FUSE at mount time. The shipped
projgit-cli (commit 9441265) does this when `--allow-other` is set.
fuser maps `MountConfig::acl = SessionACL::All` to the kernel
`allow_other` option automatically.

For non-root users, the host kernel additionally requires
`user_allow_other` in `/etc/fuse.conf`:

```
# /etc/fuse.conf
user_allow_other
```

Without it, `fusermount` refuses the mount with
`option allow_other only allowed if 'user_allow_other' is set` and the
mount never comes up. This is host configuration, not something the
projgit binary can fix from inside.

### What "with `--allow-other`" actually unblocks

Same mount, same experiments, after re-mounting with `--allow-other`
(and `user_allow_other` enabled):

```
$ target/release/projgit mount /workspaces/projgit /tmp/mp-exp \
    --ref main --offline --allow-other
$ grep mp-exp /proc/self/mountinfo
… fuse.projgit projgit ro,user_id=1000,group_id=1000,allow_other

$ sudo -n ls /tmp/mp-exp
Cargo.lock  Cargo.toml  crates  docs  LICENSE-APACHE …

$ sudo -n unshare --mount bash -c '
    mount --bind /tmp/mp-exp /mnt/x
    ls /mnt/x
  '
Cargo.lock  Cargo.toml  crates  docs  LICENSE-APACHE …

$ sudo -n git -C /tmp/mp-exp rev-parse HEAD
56babe6bbca9fa998b100b55e3f1a11d2f404d79
$ sudo -n git -C /tmp/mp-exp log --oneline -n 3
56babe6 docs(handoff): close B1+B2 + add Phase C to next-up list
d58603d docs(bench): refresh baseline with cargo + sequential numbers
a0a054c bench: add --scenario sequential to validate workload §1.6
```

Three results worth recording:

1. **Cross-UID access works.** Mount table shows `allow_other`; root
   can read; bind-into-namespace + root reading the bind works.
2. **`safe.directory` doesn't trip.** `git rev-parse HEAD` and `git
   log` succeed as root inside the mount, because the per-op uid echo
   (see §4) makes the mount root appear root-owned to root.
3. **The bind-mount mechanism is namespace-clean.** The unshared
   mount's `/proc/self/mountinfo` shows the same FUSE superblock
   (`0:140` in the test run) at both paths — exactly the shape Docker
   `-v` creates.

The full experiment log lives in git history at this commit.

## 3. What's verified vs what's still open

### Verified 2026-05-18 (T1 baseline)

- `--allow-other` is necessary and sufficient for cross-UID access
  at the FUSE-driver layer.
- Binding a FUSE mount into a different mount namespace works at the
  kernel level (`unshare --mount` proxies for Docker `-v`).
- `git -C <mount>` works inside the mount for the user that read it
  (no `safe.directory` failure).
- `MountConfig::acl = SessionACL::All` translates correctly to the
  kernel `allow_other` mount option via fuser.

### Open / not yet verified

The §5 list below is the canonical "what's left."

## 4. Open issue: attribute-cache vs per-op uid echo

The FUSE adapter sets each file's apparent owner per request from
`req.uid()` / `req.gid()` (Gotcha #9 in
[`../implementation/handoff.md`](../implementation/handoff.md)).
**But** the kernel caches attribute lookups for `ATTR_TTL` after the
first `lookup` / `getattr` reply — see
[`crates/projgit-fuse/src/adapter.rs`](../../crates/projgit-fuse/src/adapter.rs);
`ATTR_TTL` is currently `86_400` seconds (one day).

Empirical consequence:

```
# vscode reads Cargo.toml first
$ ls -la /tmp/mp-exp/Cargo.toml
-rw-r--r-- 1 vscode vscode 1122 May 18 04:20 /tmp/mp-exp/Cargo.toml

# then root reads — gets the cached attrs, sees vscode-owned
$ sudo -n ls -la /tmp/mp-exp/Cargo.toml
-rw-r--r-- 1 vscode vscode 1122 May 18 04:20 /tmp/mp-exp/Cargo.toml
```

Reverse the order (root reads first) and *everyone* sees `root:root`
for up to a day. The per-op echo is correctly applied at the adapter
layer; the *kernel* is caching the response and serving it to
subsequent readers regardless of UID.

For containers, the practical implication: in a shared mount, the
first reader after the mount comes up effectively defines the apparent
owner of every touched file for `ATTR_TTL`. In the canonical T1
deployment this is fine — see §5.1 below, where the empirical smoke
test shows the in-container UID is the first reader of nested inodes
and gets the cache populated correctly for its own use.

Possible future fixes if a multi-reader-different-UID scenario ever
surfaces, in roughly increasing complexity:

- **`--uid <N> --gid <N>` flag** that bypasses per-op echo and sets
  fixed ownership. Trivial. Best matches a sysadmin model where
  multiple consumers want to see the same owner regardless of who
  asked first.
- **Shorter `ATTR_TTL`.** Cheap. Trades cache hit rate for ownership
  freshness. Projections are immutable, so the cache buys a lot —
  trimming it has real performance cost.
- **Custom kernel-cache invalidation** via fuser's notify channel
  when we see access from a new UID. Most correct, most complex.

Not blocking T1 today.

## 5. Open issues and follow-ups

### 5.1 Non-root user inside the container (untested)

### 5.1 Non-root user inside the container — **RESOLVED 2026-05-18**

Originally an open question: would `--allow-other` alone be enough
for the typical Docker security practice of running containers as a
non-root UID, or would we also need a `--uid` flag (the hypothesis
from §4)?

**Empirically resolved with a non-root smoke test inside a fresh
mount namespace.** `unshare --mount + mount --bind /tmp/mp-exp` is the
exact kernel mechanism Docker's `-v` uses; `setpriv --reuid=5000
--regid=5000 --clear-groups` proxies for the in-container UID. The
mount was established by `vscode` (uid 1000) with `--allow-other`;
UID 5000 was the first reader of any nested inode.

```text
--- whoami as UID 5000 ---
uid=5000 gid=5000 groups=5000

--- ls -la Cargo.toml as UID 5000 ---
-rw-r--r-- 1 5000 5000 1122 May 18 05:13 /mnt/repo/Cargo.toml

--- ls -la .git as UID 5000 (safe.directory cares about this) ---
-rw-r--r-- 1 5000 5000   93 May 18 05:13 config
-rw-r--r-- 1 5000 5000   41 May 18 05:13 HEAD
-rw-r--r-- 1 5000 5000 6560 May 18 05:13 index
drwxr-xr-x 2 5000 5000    0 May 18 05:13 objects

--- git rev-parse HEAD as UID 5000 ---
c3c3c79f62de8aa939b8fe1d01992f7fed3aefde

--- git log -n 3 as UID 5000 ---
c3c3c79 (HEAD) docs(design): container deployment topology + experiment results
9441265 feat(cli): add --allow-other for cross-UID / container access
56babe6 docs(handoff): close B1+B2 + add Phase C to next-up list
```

`.git/HEAD`, `.git/config`, `.git/index`, and `.git/objects/` all
appear owned by `5000:5000`. `safe.directory` does *not* trip.
`git rev-parse HEAD` and `git log` both succeed.

**Why it works in this case:** §4's attr-cache observation is "the
first reader's UID wins for ATTR_TTL." In a real container
deployment, the typical sequence is: projgit mounts (as some
`projgit-svc` UID, no nested reads yet), the container starts and
reads via the bind, the container *is* the first reader of those
inodes. The cache populates with the container's UID and serves it
back to that container for the rest of its short life. The §4 finding
is a non-issue for the canonical T1 deployment.

**Caveats this resolution does NOT cover:**

- **Multiple containers as different UIDs reading the same mount.**
  The first reader still wins ATTR_TTL-wide; the second container
  (different UID) would see files owned by the first. For a single
  shared service account this is a non-issue. For multi-tenant T3
  it would matter — but T3 has bigger open questions (Phase C).
- **`projgit-svc` reading the mount itself** (e.g. a monitoring
  script that `ls` the mount). That would populate the cache as
  `projgit-svc`-owned and the container would see those attrs
  until `ATTR_TTL` expires. Easy operational rule: the supervising
  process should not stat the mount contents itself.

**Implication:** the `--uid` flag hypothesized in §4 is not needed
for the canonical T1 deployment. Keep it in the future-fixes list
only if a multi-reader scenario surfaces.

### 5.2 Mount propagation if projgit restarts

Docker's default `-v` bind uses `rprivate` propagation. With `rprivate`,
if projgit dies and re-mounts, the container's bind points at the
underlying empty directory the new FUSE mount layered over —
container sees an empty dir until restart. `bind-propagation=rslave`
on the `-v` and `--make-shared` on the host mountpoint fix this. Pure
deployment recipe, no projgit code change.

### 5.3 Cross-process single-flight is unmeasured (T3)

N containers cold-faulting the same OID will each fork `git fetch
<oid>` against the same `.git/objects/pack/`. Git's own locking
prevents *corruption*, but you may pay N× the network cost. That's
the audit A3 finding; the [bench Phase C](../bench/baseline.md) item
in the [handoff](../implementation/handoff.md) "What I'd do next" is
the next-up to put a number on it.

If Phase C shows N× is bad, the fix is either:

- **File-lock in the cache dir keyed by OID being fetched** —
  lightweight, ~100 LOC in `HydratingObjectStore`.
- **`projgitd` daemon** that mediates fetches across all mounts on
  the host. The architecture the problem-statement always assumed
  for §1.6 multiplexing — see audit A1. Larger, but the right
  long-term answer.

### 5.4 Rootless / Podman

User namespaces + FUSE has had real kernel bugs as recently as 2024.
Worth scoping out separately if rootless is a deployment target.
We haven't tested it.

### 5.5 Read-only invariant still applies

Containers writing into the mount get `EROFS` from us today. Build
tools (`cargo build`, `pytest`, `npm install`) want a writable
working tree; the typical fix is to sandwich an overlayfs on top of
the projgit mount. That's the audit A4 finding — a deployment
recipe to document, not a projgit code change.

## 6. Security model

`--allow-other` is a deliberate choice with a real consequence: any
local user on the host (or any container with a bind-mount) can read
the projection. Right defaults:

- **Single-tenant agent-eval rig** (every container is "yours",
  every projection is non-sensitive): `--allow-other` is fine.
- **Multi-tenant host** where projections may contain private code:
  `--allow-other` is wrong — anyone who can `stat` the path can read
  it. Don't enable until projgit has per-mount access control (which
  it doesn't today).

The CLI flag's doc comment carries the security note so the help
text is enough to make the decision.

## 7. Recommendation: smallest credible "deploy projgit in containers" path

1. **`--allow-other` flag** — done (commit 9441265, 2026-05-18).
2. **Non-root container user smoke test** — done (§5.1, resolved
   2026-05-18). `--allow-other` alone is sufficient; the `--uid`
   flag from §4 is not needed for the canonical T1 case.
3. **`projgit mount --background` + `projgit umount`** — already on
   the [handoff](../implementation/handoff.md) "What I'd do next" list.
   Required for any real deployment because the foreground process
   currently owns the mount.
4. **Document deployment recipe** in `docs/`:
   `/etc/fuse.conf`, `bind-propagation`, a sample systemd unit, an
   example Docker invocation. Sensible after `--background` lands so
   the recipe references a real supervised mount path.
5. **Phase C bench** — settles whether T3 is viable as-is or needs
   A3.
6. **Cross-process single-flight (A3) or `projgitd`** — only if
   Phase C says it's needed.

The order matters: each step's outcome informs the next, and stopping
at any point still leaves a coherent story.

## 8. What this document is not

- A user-facing deployment guide. That's the §7 follow-up; this doc
  is the architectural framing.
- A scaling guide. The §1.6 amortisation is qualitative; exact
  concurrency limits depend on deployment and aren't fixed by
  projgit.
- A promise that T3 works. T3 needs Phase C data before we can
  claim it.
