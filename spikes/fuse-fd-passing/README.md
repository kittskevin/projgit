# Stage 0 spike — FUSE fd passing

> **Outcome: GREEN.** 2026-05-20.
>
> A process that did NOT open `/dev/fuse` and did NOT call `mount(2)`
> can fully serve the FUSE protocol on the resulting fd received via
> `SCM_RIGHTS`, using fuser's `Session::from_fd`. **Stage 4 of the
> projgitd plan is viable.**

This is throwaway code; not a workspace member, not shipped. Purpose:
settle the load-bearing assumption for the `projgitd` plan
([`docs/design/projgitd.md`](../../docs/design/projgitd.md) §8,
Stage 0).

## What was tested

The architectural property: **process A opens `/dev/fuse` + does
`mount(2)`, hands the fd to process B over a unix socket, then exits;
process B serves the FUSE protocol against the received fd.**

In the projgitd deployment, A = Harbor (privileged orchestrator),
B = sidecar (per-container, holds the FUSE fd, runs the protocol
loop locally). The agent never participates.

## Concrete sequence

```sh
# terminal 1: start serve (waits on unix socket for an fd)
cargo run --release -- serve /tmp/spike.sock

# terminal 2: open + send fd (needs root because of raw mount(2))
mkdir -p /tmp/spike-mp
sudo cargo run --release -- open /tmp/spike-mp /tmp/spike.sock
# → open exits cleanly; mount stays in the kernel namespace

# terminal 3: kernel-side verify
ls -la /tmp/spike-mp        # → "hello" with size 6, root-owned
cat /tmp/spike-mp/hello     # → "world"

# cleanup
sudo fusermount3 -u /tmp/spike-mp
# → serve's BackgroundSession::join returns; serve exits cleanly
```

Captured output (2026-05-20 in the projgit devcontainer):

```
[open] opening /dev/fuse
[open] mount(spike, /tmp/spike-mp, fuse, MS_NOSUID|MS_NODEV,
       "fd=3,rootmode=40000,user_id=0,group_id=0,allow_other")
[open] mounted; fd=3 uid=0 gid=0
[open] connecting to /tmp/spike.sock
[open] sending fd via SCM_RIGHTS
[open] sent. closing local fd, exiting. mount stays in namespace.

[serve] listening on /tmp/spike.sock; waiting for `open` to connect
[serve] accepted; awaiting fd
[serve] received fd=5; wrapping with Session::from_fd
[serve] handshake ok; entering protocol loop
[serve] background session running; waiting on join

# from a third shell:
$ ls -la /tmp/spike-mp
total 33
drwxr-xr-x 2 root root     0 Jan  1  1970 .
drwxrwxrwt 1 root root 28672 May 20 06:55 ..
-rw-r--r-- 1 root root     6 Jan  1  1970 hello

$ cat /tmp/spike-mp/hello
world

# after fusermount3 -u:
[serve] joined; serve exiting
```

## Findings

### 1. `Session::from_fd` works exactly as documented

[`fuser::Session::from_fd`](https://docs.rs/fuser/0.17.0/fuser/struct.Session.html#method.from_fd)
takes an `OwnedFd` + `SessionACL` + `Config`, does the
`FUSE_INIT` handshake against the kernel, and returns a `Session`
that can be driven via the standard `Session::spawn() ->
BackgroundSession::join()` path. No private APIs needed; no fork
of fuser required.

### 2. The mount is decoupled from the opener

`mount(2)` registers a mount in the current mount namespace. The
mount stays there until explicitly unmounted, regardless of whether
the opening process is alive or holds the fd. Once `open` exits,
the kernel mount table still contains the FUSE mount, with the
`serve` process being the sole userspace holder of the
`/dev/fuse` fd.

This is the property that makes the projgitd sidecar model possible:
Harbor sets up the mount and walks away; the sidecar serves.

### 3. FUSE_INIT is read-on-demand, not pushed eagerly

Initial worry: would the kernel send `FUSE_INIT` after `mount(2)`
returns, and would `open`'s fd buffer accumulate it before being
passed to `serve`?

Empirically: no. The kernel only emits the next FUSE op when
userspace `read()`s. `open` never reads the fd (just opens, mounts,
passes, exits). When `serve` calls `Session::from_fd`, the kernel
delivers `FUSE_INIT` as the first op, fuser handles it, mount is
operational.

This means **`open` must not create a fuser `Session` itself**, or
it will consume the INIT and `serve`'s handshake will hang. The
spike's `open` uses raw `nix::mount::mount(...)` + `OwnedFd` from
`/dev/fuse` and never wraps in fuser. That's the contract for
Harbor too.

### 4. Privilege placement matches the projgitd security model

- `open` requires `CAP_SYS_ADMIN` (or `fusermount3` setuid) because
  it calls `mount(2)` directly. Runs as root via `sudo` in the
  spike. In production this is Harbor.
- `serve` requires no special privileges. It just receives a fd
  and reads/writes it. This is the sidecar — runs as whatever
  unprivileged user the deployment chooses.
- The agent is not in the picture at all.

This split is what the [container-deployment
doc](../../docs/design/container-deployment.md) §5.6 and the
[projgitd design](../../docs/design/projgitd.md) §3, §6 all
described in theory. The spike confirms it actually works.

### 5. Clean teardown

When external `fusermount3 -u` runs against the mountpoint, the
kernel closes the FUSE protocol channel. fuser's
`BackgroundSession::join()` returns `Ok(())`, the serve process
exits cleanly. No leaked threads, no zombies, no manual
intervention. The supervision story is sane.

## What this does NOT validate

- **`setns` into the agent's mount namespace.** The spike mounts in
  the current namespace. A production T4 deployment would use
  `setns(CLONE_NEWNS)` (or fork inside the agent's namespace) to
  put the mount somewhere only the agent can see. That's a separate
  capability and worth its own validation in Stage 4.
- **Rootless containers.** Tested only as root (via sudo). User
  namespaces + FUSE have had real kernel bugs; rootless is its
  own adventure if it becomes a deployment target.
- **Multi-fd / fd cloning.** Only one fd is passed; the spike doesn't
  exercise fuser's `FUSE_DEV_IOC_CLONE` ioctl path. Not needed for
  the immediate plan.

## Implications for Stages 1–5

- **Stage 1** (multi-projection in one process): unaffected. Proceeds
  on the established path.
- **Stage 2** (daemon scaffold + DaemonFetcher): unaffected. Daemon
  initially hosts mounts itself; fd passing comes in later stages.
- **Stage 3** (sidecar holds fd): now known viable. The sidecar can
  receive a fd from any source and wrap it in `Session::from_fd`.
- **Stage 4** (T4 last mile, per-namespace mount): **green-lit.**
  The Harbor → sidecar fd handoff is exactly what this spike
  proved, modulo the namespace-targeting work (`setns` or
  fork-into-namespace).
- **Stage 5** (lifecycle / supervision): clean teardown semantics
  (point 5 above) make systemd-style supervision straightforward.

## How to dispose of this spike

This crate is throwaway. When Stage 4 ships and the production sidecar
implements the same pattern, this spike can be deleted (its findings
live in the design doc + this README; the spike code itself has no
ongoing value).

Until then it lives in `spikes/fuse-fd-passing/` so we can re-run the
demo if the toolchain or kernel changes in a way that might break the
mechanism.
