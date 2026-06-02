#!/usr/bin/env bash
# scripts/docker-smoke/run.sh — exercise the Stage 3 projgitd
# sidecar topology with real docker containers.
#
# Topology (matches docs/design/projgitd.md §1):
#
#   ┌──────────────────────┐       ┌───────────────────────┐
#   │  projgitd container  │◀──────│  sidecar container    │
#   │   (data plane)       │ socket│  (FUSE fd + protocol) │
#   │   /sock/projgitd.sock│       │  mounts /mnt/repo     │
#   │   /cache/<repo>      │◀──────│  reads /cache/<repo>  │
#   └──────────────────────┘ shared└───────────────────────┘
#                            volume        │
#                                          │  docker exec sidecar ls /mnt/repo
#                                          ▼
#                            ┌────────────────────────────┐
#                            │  this script's verify step │
#                            └────────────────────────────┘
#
# The sidecar's FUSE mount lives in its own container mount namespace.
# A third container with `-v <sidecar-mnt>` would see nothing — that's
# the property; we verify via `docker exec` which enters the sidecar's
# namespace.
#
# Usage (run from anywhere; uses repo root resolved from this file):
#   scripts/docker-smoke/run.sh                       # mount this repo
#   scripts/docker-smoke/run.sh /path/to/other/repo   # mount a different local repo
#
# Requires: docker on the PATH (NOT available inside the projgit
# devcontainer; run this from the host). Builds release binaries on
# the host before launching so the image stays toolchain-free.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SOURCE="${1:-${REPO_ROOT}}"
IMAGE_TAG="projgit-smoke:latest"

# Per-invocation shared workspace on the host. Holds the daemon
# socket and the on-disk CAS so daemon + sidecar can both reach them.
WORK=$(mktemp -d -t projgit-docker-smoke.XXXXXX)
trap 'cleanup' EXIT

cleanup() {
    set +e
    echo
    echo "--- cleanup ---"
    docker rm -f projgit-smoke-sidecar projgit-smoke-daemon 2>/dev/null
    rm -rf "${WORK}"
}

echo "--- [1/5] checking prerequisites ---"
command -v docker >/dev/null || { echo "docker not on PATH"; exit 1; }
[[ -e /dev/fuse ]] || { echo "/dev/fuse missing on host"; exit 1; }

echo "--- [2/5] building release binaries ---"
(cd "${REPO_ROOT}" && cargo build --release --bin projgit --bin projgitd)

echo "--- [3/5] building runtime image ${IMAGE_TAG} ---"
docker build -t "${IMAGE_TAG}" "$(dirname "$0")"

echo "--- [4/5] starting daemon + sidecar containers ---"
mkdir -p "${WORK}/sock" "${WORK}/cache"

# Daemon: no FUSE, no privileged caps. Just listens on the unix
# socket and owns the on-disk CAS.
docker run -d --rm \
    --name projgit-smoke-daemon \
    -v "${REPO_ROOT}/target/release/projgitd:/usr/local/bin/projgitd:ro" \
    -v "${WORK}/sock:/sock" \
    -v "${WORK}/cache:/cache" \
    -v "${SOURCE}:/source:ro" \
    "${IMAGE_TAG}" \
    /usr/local/bin/projgitd --socket /sock/projgitd.sock --socket-mode 0666 \
    >/dev/null

# Sidecar: needs FUSE caps to mount(2). Shares the socket and cache
# volumes with the daemon. Mounts FUSE inside its own container
# mount namespace at /mnt/repo.
docker run -d --rm \
    --name projgit-smoke-sidecar \
    --cap-add=SYS_ADMIN \
    --device=/dev/fuse \
    --security-opt=apparmor:unconfined \
    -v "${REPO_ROOT}/target/release/projgit:/usr/local/bin/projgit:ro" \
    -v "${WORK}/sock:/sock" \
    -v "${WORK}/cache:/cache" \
    -v "${SOURCE}:/source:ro" \
    "${IMAGE_TAG}" \
    /usr/local/bin/projgit mount --daemon-socket /sock/projgitd.sock \
        --ref main --no-dotgit /source /mnt/repo \
    >/dev/null

# Wait for the sidecar's FUSE mount to come up inside its namespace.
# We can't `stat` from the host (mount is in the sidecar's mount-ns);
# `docker exec ... mountpoint` enters the namespace.
echo "--- waiting for FUSE mount inside sidecar namespace ---"
for i in $(seq 1 30); do
    if docker exec projgit-smoke-sidecar mountpoint -q /mnt/repo 2>/dev/null; then
        break
    fi
    sleep 0.5
    if [[ $i -eq 30 ]]; then
        echo "FUSE mount never came up; sidecar logs:"
        docker logs projgit-smoke-sidecar
        exit 1
    fi
done

echo "--- [5/5] verifying through the kernel mount ---"
echo
echo "1. ls /mnt/repo (top-level entries):"
docker exec projgit-smoke-sidecar ls /mnt/repo | sed 's/^/    /' | head -10
echo
echo "2. head -3 /mnt/repo/Cargo.toml (proves cold-fetch through DaemonFetcher):"
docker exec projgit-smoke-sidecar head -3 /mnt/repo/Cargo.toml | sed 's/^/    /'
echo
echo "3. daemon status (via attach RPC over the shared socket):"
docker exec projgit-smoke-daemon \
    /usr/local/bin/projgitd --help >/dev/null 2>&1  # sanity
docker exec projgit-smoke-sidecar \
    /usr/local/bin/projgit attach --socket /sock/projgitd.sock status \
    | sed 's/^/    /'
echo
echo "--- PASS: stage 3 sidecar topology serves files through docker containers ---"
