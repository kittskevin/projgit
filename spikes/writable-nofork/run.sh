#!/usr/bin/env bash
# Phase 2 no-fork spike driver.
#
# Builds a virtual worktree over a git commit (via the `vworktree` FUSE
# harness) and drives STOCK git against it — read-tree / status / add /
# commit — with NO core.virtualFilesystem, measuring how much file
# content git hydrates (lower-layer read() calls) at each step.
#
# Usage:
#   ./run.sh                      # M1..M4 on a small repo
#   NFILES=20000 DIRS=200 ./run.sh scale   # M2 scale test
#
# Requires: git >= 2.37, /dev/fuse, fusermount.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
MODE="${1:-m1m4}"
NFILES="${NFILES:-300}"
DIRS="${DIRS:-20}"

echo "[build] cargo build --release"
( cd "$HERE" && cargo build --release >/dev/null 2>&1 ) || { echo "build failed"; exit 1; }
TARGET_DIR="$(cd "$HERE" && cargo metadata --no-deps --format-version 1 2>/dev/null \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['target_directory'])")"
BIN="$TARGET_DIR/release/vworktree"
HOOK="$HERE/fsmonitor-hook.sh"
[ -x "$BIN" ] || { echo "binary not found at $BIN"; exit 1; }

chmod +x "$HOOK"

WORK="$(mktemp -d)"
SRC="$WORK/src"
SERVED="$WORK/served"
MNT="$WORK/mnt"
FSM="$WORK/fsmonitor-log"
GD="$SERVED/.git"
mkdir -p "$MNT"

HARNESS_PID=""
cleanup() {
  [ -n "$HARNESS_PID" ] && kill "$HARNESS_PID" 2>/dev/null || true
  fusermount -u "$MNT" 2>/dev/null || fusermount3 -u "$MNT" 2>/dev/null || true
  sleep 0.3
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

# ---- stock git, pinned to our git-dir + the virtual worktree ----------
gitw() { git --git-dir="$GD" --work-tree="$MNT" -c safe.directory="$MNT" "$@"; }

read_counter() { awk -v k="$1" '$1==k{print $2}' "$MNT/.nofork-stats" 2>/dev/null; }

declare -A SNAP
snap() { for k in reads upper_reads hydrations getattrs lookups readdirs writes creates materializations; do SNAP[$k]=$(read_counter "$k"); done; }
delta() { echo $(( $(read_counter "$1") - ${SNAP[$1]:-0} )); }
report() {
  printf '  %-26s reads(hydrate)=%-6s getattr=%-7s readdir=%-5s writes=%-4s creates=%-4s materialize=%-4s\n' \
    "$1" "$(delta reads)" "$(delta getattrs)" "$(delta readdirs)" "$(delta writes)" "$(delta creates)" "$(delta materializations)"
}

# ---- make a source repo with NFILES files across DIRS dirs ------------
mk_repo() {
  echo "[setup] creating source repo: $NFILES files / $DIRS dirs"
  git init -q "$SRC"
  git -C "$SRC" config user.email a@b.c
  git -C "$SRC" config user.name spike
  git -C "$SRC" config commit.gpgsign false
  python3 - "$SRC" "$NFILES" "$DIRS" <<'PY'
import os, sys
root, n, dirs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
for i in range(n):
    d = os.path.join(root, f"dir{i % dirs:03d}")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"file{i:06d}.txt"), "w") as f:
        f.write(f"content of file {i}\nline two\nline three\n")
PY
  git -C "$SRC" add -A
  git -C "$SRC" commit -qm init
  echo "[setup] cloning --no-checkout -> served (.git only, no worktree files)"
  git clone -q --no-checkout "file://$SRC" "$SERVED"
}

mount_harness() {
  echo "[setup] mounting virtual worktree at $MNT"
  VWORKTREE_FSM="$FSM" "$BIN" --repo "$GD" --commit HEAD --mount "$MNT" \
    --fsmonitor-file "$FSM" --ready-file "$WORK/ready" 2>"$WORK/harness.log" &
  HARNESS_PID=$!
  for _ in $(seq 1 100); do [ -f "$WORK/ready" ] && break; sleep 0.1; done
  [ -f "$WORK/ready" ] || { echo "MOUNT FAILED"; cat "$WORK/harness.log"; exit 1; }
}

echo "=================================================================="
echo " Phase 2 no-fork spike  (git $(git --version | awk '{print $3}'))"
echo " mode=$MODE  files=$NFILES  dirs=$DIRS"
echo "=================================================================="
mk_repo
mount_harness

echo
echo "[sanity] virtual worktree is browsable without hydration:"
snap
ls "$MNT" >/dev/null
find "$MNT" -type d >/dev/null
report "ls + find -type d"
echo "  (a pure directory walk reads tree metadata only; reads should be 0)"

# ----------------------------------------------------------------------
# M1 — does `git status` hydrate? (no FSMonitor, no sparse-index)
# ----------------------------------------------------------------------
echo
echo "## M1 — git status on the virtual worktree (stock git, no fork)"
gitw config core.fsmonitor false
gitw config core.untrackedCache false

snap; gitw read-tree HEAD; report "read-tree HEAD"

snap; gitw status --porcelain >/dev/null; report "status #1 (naive index)"
snap; gitw status --porcelain >/dev/null; report "status #2 (warm index)"

echo "  -- with core.checkStat=minimal + index refresh --"
gitw config core.checkStat minimal
snap; gitw update-index --refresh >/dev/null 2>&1 || true; report "update-index --refresh"
snap; gitw status --porcelain >/dev/null; report "status #3 (post-refresh)"

CLEAN=$(gitw status --porcelain | wc -l)
echo "  -- correctness: 'git status' reports $CLEAN changed entries (expect 0 = clean)"

# ----------------------------------------------------------------------
# M3 — FSMonitor answered from the harness write log
# ----------------------------------------------------------------------
echo
echo "## M3 — FSMonitor (daemon-style) makes status skip scanning"
gitw config core.fsmonitorHookVersion 2
gitw config core.fsmonitor "$HOOK"
# Persist the fsmonitor baseline into the index (status --porcelain alone
# is read-only and won't store the token), then settle:
gitw update-index --refresh >/dev/null 2>&1 || true
gitw status --porcelain >/dev/null 2>&1 || true
snap; gitw status --porcelain >/dev/null; report "status (fsmonitor steady)"
echo "  (getattr -> sharply lower means git trusted the monitor, skipped lstat scanning)"

# ----------------------------------------------------------------------
# M4 — materialize-on-write: edit, status, add, commit
# ----------------------------------------------------------------------
echo
echo "## M4 — edit a file in the mount, then git add + git commit"
TARGET="dir000/file000000.txt"
echo "  editing $TARGET inside the virtual mount..."
snap
echo "APPENDED-BY-SPIKE" >> "$MNT/$TARGET"
sync
report "write (materialize)"
echo "  read-back last line: '$(tail -1 "$MNT/$TARGET")'  (expect APPENDED-BY-SPIKE)"
echo "  harness write-log now reports $(tr '\0' '\n' < "$FSM" | tail -n +2 | grep -c . || true) changed path(s)"

# Authoritative detection via a normal stat scan (fsmonitor off):
CH_NOFS="$(gitw -c core.fsmonitor=false status --porcelain || true)"
# FSMonitor detection (one settle query absorbs git's documented post-change lag):
gitw status --porcelain >/dev/null 2>&1 || true
CH_FS="$(gitw status --porcelain || true)"
echo "  git status (normal scan):  ${CH_NOFS:-<clean>}"
echo "  git status (fsmonitor):    ${CH_FS:-<clean>}"
NCH=$(printf '%s\n' "$CH_NOFS" | grep -c . || true)

gitw add "$TARGET"
if gitw -c user.email=a@b.c -c user.name=spike -c commit.gpgsign=false commit -qm "spike: edit $TARGET"; then
  NEWHEAD=$(gitw rev-parse HEAD)
  BLOB=$(gitw cat-file -p "HEAD:$TARGET" | tail -1)
  echo "  committed -> HEAD=$NEWHEAD"
  echo "  committed file last line: '$BLOB'  (expect APPENDED-BY-SPIKE)"
else
  echo "  COMMIT FAILED (nothing staged?)"
  BLOB="<none>"
fi

echo
echo "## verdict inputs"
echo "  M1 clean status changed-entries: $CLEAN  (0 = git sees clean virtual worktree)"
echo "  M4 modified-entries after edit:  $NCH    (1 = exactly the edited file)"
echo "  M4 commit verify last line:      '$BLOB'"
echo
echo "Final harness counters:"
sed 's/^/  /' "$MNT/.nofork-stats"

if [ "$MODE" = "scale" ]; then
  echo
  echo "## M2 — scale + sparse-index (this run: $NFILES files)"
  ms() { date +%s%3N; }
  gitw config core.fsmonitor false
  gitw update-index --refresh >/dev/null 2>&1 || true
  snap; t0=$(ms); gitw status --porcelain >/dev/null; t1=$(ms)
  echo "  full-index   status: $((t1-t0)) ms   hydration(read)=$(delta reads)   getattr=$(delta getattrs)   index=$(du -h "$GD/index" | awk '{print $1}')"

  echo "  enabling sparse-index (cone = dir000)..."
  gitw sparse-checkout init --cone --sparse-index >/dev/null 2>&1 || gitw sparse-checkout init --cone >/dev/null 2>&1 || true
  gitw sparse-checkout set dir000 >/dev/null 2>&1 || true
  gitw update-index --refresh >/dev/null 2>&1 || true
  snap; t2=$(ms); gitw status --porcelain >/dev/null; t3=$(ms)
  echo "  sparse-index status: $((t3-t2)) ms   hydration(read)=$(delta reads)   getattr=$(delta getattrs)   index=$(du -h "$GD/index" | awk '{print $1}')"
  echo "  -> sparse-index shrinks the on-disk index and narrows the scan, no fork."
fi

echo
echo "[done] (work dir $WORK cleaned on exit)"
