# projgit devcontainer

This devcontainer gives `projgit-fuse` a working Linux + FUSE
environment so we can actually mount and serve filesystems from
Rust — something the Windows host can't do directly.

## Quick start

1. Install [Docker Desktop](https://www.docker.com/products/docker-desktop/)
   (Windows / macOS) or Docker Engine (Linux) and the
   **Dev Containers** VS Code extension.
2. Open this repo in VS Code.
3. Command palette → **Dev Containers: Reopen in Container**.
4. Wait for `postCreateCommand` to install fuse3, libfuse3-dev,
   pkg-config, debuggers, and rustup components. ~1–2 minutes the
   first time, instant on subsequent opens.
5. Verify the toolchain is happy:

   ```sh
   cargo build --workspace
   cargo test  -p projgit-core
   ```

## Run the FUSE mount smoke test

```sh
cargo test -p projgit-fuse --test mount_smoke -- --ignored --nocapture
```

The test is `#[ignore]`-gated because FUSE isn't available on every
host (Windows native, CI runners without `/dev/fuse`, etc.). Inside
this devcontainer it should pass; outside, it should be skipped.

If it fails with "Operation not permitted" or similar, double-check
the container was started with `--cap-add=SYS_ADMIN` and
`--device=/dev/fuse` (these come from `runArgs` in
`devcontainer.json`; rebuild the container if you've edited that
file).

## Why is the container privileged?

FUSE mounts require `SYS_ADMIN` and access to `/dev/fuse`. We also
set `apparmor:unconfined` to bypass Docker's default AppArmor
profile, which blocks mount syscalls. This is the standard recipe
for FUSE-in-container; see the [Docker FUSE
docs](https://docs.docker.com/engine/security/seccomp/) for details.

The container only ever holds projgit's source + a Linux
toolchain. It does not have host root or any other elevated
privileges beyond what FUSE needs.

## `target/` lives in a Docker volume

The workspace itself is bind-mounted from the host, but `target/`
is a Docker **named volume** (`projgit-cargo-target`). On Windows
hosts the workspace bind-mount is NTFS-backed and cargo's many
tiny I/O operations are pathologically slow there. Volume-backed
`target/` is ~10× faster on warm rebuilds.

The volume is created root-owned on first mount, so
`postCreateCommand` runs `sudo chown vscode:vscode target/` once;
that fix persists inside the volume across container rebuilds.

Implications:
- `cargo clean` from the host **does not** wipe build artifacts.
  Run it from inside the container, or:
- Reset everything with `docker volume rm projgit-cargo-target`
  (container must be stopped first).
- The artifacts inside the volume are Linux ELF binaries, not
  consumable from the Windows host.

## Host configuration notes

The same `devcontainer.json` works on Windows, macOS, and Linux
hosts:

| Host | Bind mount | `target/` volume needed? |
|---|---|---|
| Windows (Docker Desktop / WSL2) | NTFS, slow | **Yes** (perf) |
| macOS (Docker Desktop / VirtioFS) | Fast | Optional, kept for parity |
| Linux native | Fast | Optional, kept for parity |

The `~/.gitconfig` bind-mount uses `${localEnv:USERPROFILE}` and
`${localEnv:HOME}` concatenated; whichever is empty on your host
collapses cleanly. If you don't have a `~/.gitconfig` on the host
the container still starts, but `git commit` from inside the
container will warn about missing user.name / user.email.

## Toolchain version

The base image (`mcr.microsoft.com/devcontainers/rust:1-bookworm`)
ships current stable Rust. Workspace MSRV is **1.85**. If the base
image ever regresses below 1.85, add a `rustup default 1.85` line
to `postCreateCommand`.
