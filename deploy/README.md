# Deploying `projgitd`

Operator cookbook for running the projgit daemon as a long-lived
service. The daemon is a **pure data plane**: it owns the upstream
git connection, the shared content-addressable store (one partial
clone per source), and the in-flight fetch coalescer. Agent
containers / sidecars run the FUSE mount themselves and talk to the
daemon over a unix socket
(`projgit mount --daemon-socket <PATH> …`).

- **Architecture + threat model:** [`../docs/design/projgitd.md`](../docs/design/projgitd.md)
  and [`../docs/design/container-deployment.md`](../docs/design/container-deployment.md) §6.
- **Runnable container example:** [`../scripts/docker-smoke/`](../scripts/docker-smoke/).

## 1. Build + install

```sh
cargo build --release -p projgit-cli -p projgit-daemon
sudo install -m 0755 target/release/projgitd /usr/local/bin/
sudo install -m 0755 target/release/projgit  /usr/local/bin/
```

## 2. Run under systemd (system service)

[`projgitd.service`](projgitd.service) is a ready-to-edit system
unit (runs as a dedicated `projgit` user; socket in
`/run/projgitd/`, cache in `/var/cache/projgit`).

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin projgit
sudo cp deploy/projgitd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now projgitd
```

### Rootless / per-operator (user service)

For the single-operator shared-host shape (Harbor's Scenario A),
run it as your own user instead:

```sh
mkdir -p ~/.config/systemd/user
cp deploy/projgitd.service ~/.config/systemd/user/
# Edit: drop User=/Group=, RuntimeDirectory=/CacheDirectory= and the
# absolute --socket/--cache-dir paths; let projgitd use its defaults
# ($XDG_RUNTIME_DIR/projgitd.sock and $XDG_CACHE_HOME/projgit).
systemctl --user daemon-reload
systemctl --user enable --now projgitd
```

The daemon's defaults already match the user-session locations, so
the rootless unit can be as short as `ExecStart=/usr/local/bin/projgitd`.

## 3. Letting other users connect

The socket defaults to mode `0600` (owner-only). On a host where
agent processes run as a **different** user than the daemon, widen
it and gate access by group:

```ini
# in the unit's [Service]
ExecStart=/usr/local/bin/projgitd --socket /run/projgitd/projgitd.sock \
    --cache-dir /var/cache/projgit --socket-mode 0660
```

The socket inherits the unit's `Group=` (via `RuntimeDirectory=`),
so add the consuming users to the `projgit` group. This is a
deliberate widening — see `docs/implementation/projgitd-plan.md`
§2.2 for the authentication rationale.

## 4. Health check

`projgit attach … ping` is the liveness probe: it connects, sends
`Ping`, and exits `0` on `Pong` or non-zero (with a clear "is the
daemon running?" message) if the socket is dead.

```sh
projgit attach --socket /run/projgitd/projgitd.sock ping  # -> "pong", exit 0
```

Use it as a systemd `ExecStartPost` readiness gate, a Kubernetes
`exec` liveness probe, or a Docker `HEALTHCHECK`. `projgit attach …
status` additionally reports uptime, the bound source, mount count,
and cache hit/miss counters.

## 5. Logs

The daemon logs to stderr via `tracing`, so journald captures it:

```sh
journalctl -u projgitd -f                 # system service
journalctl --user -u projgitd -f          # user service
```

Default level is `info`. Raise it with `-v` (`debug`) / `-vv`
(`trace`) on the `ExecStart` line, or per-target without restarting
the arg surface via the environment:

```ini
Environment=PROJGIT_LOG=projgit_daemon=debug
```

(`PROJGIT_LOG` takes precedence over `-v`; `RUST_LOG` works too.)
The separate `--trace` flag emits one greppable `trace: rpc=…` line
per RPC for data-plane debugging — see
[`../docs/bench/baseline.md`](../docs/bench/baseline.md) §Diagnostic.

## 6. PID file (optional)

`--pid-file <PATH>` writes the daemon's PID once the socket is bound
(so the file's presence is also a readiness marker) and removes it
on graceful shutdown. With `Type=simple` systemd tracks the main PID
directly, so you usually don't need it; it's here for supervisors
that want an explicit PID handle. A `SIGKILL` leaves the file stale.

## 7. Restart behaviour / state

The daemon keeps no durable state of its own: the partial clone
already lives in the cache dir, and in the Stage 3 sidecar model the
daemon doesn't own any mounts (each sidecar holds its own FUSE fd).
So a restart re-`Attach`es to the same source cheaply (no re-clone)
and sidecars' warm reads keep working through the shared on-disk CAS
while the daemon is briefly down — only cold-object hydration pauses.
That's why there's no persistent-state file to manage.
