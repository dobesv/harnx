#!/usr/bin/env bash
# Render a VHS tape against a local harnx-mock-llm server.
#
# Usage: demos/render.sh <tape-name>
#   <tape-name> matches a file at demos/<tape-name>.tape and a script at
#   demos/scripts/<tape-name>-flow.yaml. Output GIF is written to
#   demos/out/<tape-name>.gif (path is set inside the .tape file).
#
# Requirements:
#   - vhs   (https://github.com/charmbracelet/vhs)
#   - cargo (the script builds harnx + harnx-mock-llm in release mode if absent)

set -euo pipefail

TAPE_NAME="${1:-agent}"
DEMOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DEMOS_DIR/.." && pwd)"
TAPE_PATH="$DEMOS_DIR/$TAPE_NAME.tape"
SCRIPT_PATH="$DEMOS_DIR/scripts/$TAPE_NAME-flow.yaml"
PORT="${HARNX_MOCK_LLM_PORT:-3829}"

if [[ ! -f "$TAPE_PATH" ]]; then
  echo "error: no tape at $TAPE_PATH" >&2
  exit 1
fi
if [[ ! -f "$SCRIPT_PATH" ]]; then
  echo "error: no mock-llm script at $SCRIPT_PATH" >&2
  exit 1
fi
if ! command -v vhs >/dev/null 2>&1; then
  echo "error: vhs not found on PATH. Install: https://github.com/charmbracelet/vhs#installation" >&2
  exit 1
fi

cd "$REPO_ROOT"

# Build release binaries if missing — VHS will type `harnx`, so it must be on
# PATH, and the agent demo spawns `harnx-mock-mcp` for its faked tool calls.
if [[ ! -x "$REPO_ROOT/target/release/harnx" ]] \
  || [[ ! -x "$REPO_ROOT/target/release/harnx-mock-llm" ]] \
  || [[ ! -x "$REPO_ROOT/target/release/harnx-mock-mcp" ]]; then
  echo "Building release binaries..."
  cargo build --release -p harnx -p harnx-test-bins
fi

export PATH="$REPO_ROOT/target/release:$PATH"
export HARNX_CONFIG_DIR="$DEMOS_DIR/config"

# Disable chromium sandbox so that we can run this inside a sandbox
export VHS_NO_SANDBOX=1

# The demos showcase syntax highlighting, so make sure color is enabled in the
# recording environment. harnx disables highlighting when NO_COLOR is set
# (env_split.rs), and resolves truecolor from COLORTERM — a clean CI/sandbox
# shell often has NO_COLOR=1 and no COLORTERM, which would render code blocks
# as flat, unhighlighted text. Present a normal color-capable terminal instead.
unset NO_COLOR
export COLORTERM=truecolor

# VHS spawns bash inside ttyd, which sources $HOME/.bashrc. Point HOME at an
# empty dir so the user's shell init doesn't leak into the recording.
CLEAN_HOME="$(mktemp -d -t harnx-demo-home.XXXXXX)"
export HOME="$CLEAN_HOME"

mkdir -p "$DEMOS_DIR/out"

# Start the mock LLM in the background and ensure it's torn down on exit.
MOCK_LOG="$(mktemp -t harnx-mock-llm.XXXXXX.log)"
harnx-mock-llm --port "$PORT" --script "$SCRIPT_PATH" >"$MOCK_LOG" 2>&1 &
MOCK_PID=$!
cleanup() {
  if kill -0 "$MOCK_PID" 2>/dev/null; then
    kill "$MOCK_PID" 2>/dev/null || true
    wait "$MOCK_PID" 2>/dev/null || true
  fi
  rm -f "$MOCK_LOG"
  rm -rf "$CLEAN_HOME"
}
trap cleanup EXIT INT TERM

# Wait for the READY line (server is single-threaded but binds before logging).
for _ in $(seq 1 30); do
  if grep -q "^READY" "$MOCK_LOG" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if ! grep -q "^READY" "$MOCK_LOG" 2>/dev/null; then
  echo "error: mock-llm did not start; log:" >&2
  cat "$MOCK_LOG" >&2
  exit 1
fi

echo "Rendering $TAPE_PATH..."
vhs "$TAPE_PATH"
echo "Done. Output: $DEMOS_DIR/out/$TAPE_NAME.gif"
