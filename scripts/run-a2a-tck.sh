#!/usr/bin/env bash
# Run from any directory. Requires Rust, Python 3.11+, uv, git and nats-server.
# On noexec Cargo registries, set PROTOC to a system protoc (locally /usr/bin/protoc).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
TCK_SHA=263b9cfaf16a554bdfb166a7ba5b67716e946349
REPORT_DIR="${A2A_TCK_REPORT_DIR:-$ROOT/target/a2a-tck-reports}"
mkdir -p "$REPORT_DIR"
REPORT_DIR="$(cd "$REPORT_DIR" && pwd)"
REPORT_DIR="$(mktemp -d "$REPORT_DIR/run-XXXXXXXX")"
printf '%s\n' "$TCK_SHA" > "$REPORT_DIR/tck-revision.txt"
echo "A2A TCK reports: $REPORT_DIR"
WORK="$(mktemp -d)"
HARNESS_PID=""
# shellcheck disable=SC2317 # Called indirectly by EXIT trap.
cleanup() {
  if [[ -n "$HARNESS_PID" ]]; then
    kill -TERM "$HARNESS_PID" 2>/dev/null || true
    wait "$HARNESS_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
cargo build --locked -p harnx-a2a-server -p harnx-worker --bins
git init -q "$WORK/tck"
git -C "$WORK/tck" remote add origin https://github.com/a2aproject/a2a-tck.git
git -C "$WORK/tck" fetch -q --depth 1 origin "$TCK_SHA"
git -C "$WORK/tck" checkout -q --detach FETCH_HEAD
test "$(git -C "$WORK/tck" rev-parse HEAD)" = "$TCK_SHA"
(cd "$WORK/tck" && uv sync --frozen)
# cargo metadata respects CARGO_TARGET_DIR and workspace target configuration.
TARGET="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
python3 "$ROOT/scripts/a2a-tck/harness.py" "$WORK" "$REPORT_DIR" "$TARGET/debug" &
HARNESS_PID=$!
STATUS=0
wait "$HARNESS_PID" || STATUS=$?
HARNESS_PID=""
exit "$STATUS"
