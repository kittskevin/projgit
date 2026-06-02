# Minimal runtime image for the docker smoke test
# (scripts/docker-smoke/run.sh).
#
# Just FUSE userspace + git + ca-certs on top of debian-slim. Binaries
# are mounted in at runtime so we don't need a Rust toolchain in the
# image and don't have to rebuild on every code change.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        fuse3 \
        git \
        ca-certificates \
 && rm -rf /var/lib/apt/lists/*
