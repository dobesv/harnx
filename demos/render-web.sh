#!/usr/bin/env bash
# Render a web-UI GIF by driving the harnx server with Playwright against a
# scripted harnx-mock-llm.
#
# Usage:  demos/render-web.sh <demo>
#   <demo> ∈ { playground, arena }
#   Reads demos/scripts/<demo>-flow.yaml and runs demos/web/<demo>.mjs.
#   Output: demos/out/<demo>.gif
#
# Requires: node, cargo, ffmpeg. Installs Playwright + chromium on first run.

set -euo pipefail

DEMO="${1:-playground}"
DEMOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DEMOS_DIR/.." && pwd)"

SCRIPT_PATH="$DEMOS_DIR/scripts/$DEMO-flow.yaml"
PLAY_SCRIPT="$DEMOS_DIR/web/$DEMO.mjs"
MOCK_PORT="${HARNX_MOCK_LLM_PORT:-3829}"
SERVE_PORT="${HARNX_SERVE_PORT:-8000}"
OUT_DIR="$DEMOS_DIR/out/$DEMO"
FINAL_GIF="$DEMOS_DIR/out/$DEMO.gif"

[[ -f "$SCRIPT_PATH" ]] || { echo "error: no mock-llm script at $SCRIPT_PATH" >&2; exit 1; }
[[ -f "$PLAY_SCRIPT" ]] || { echo "error: no Playwright script at $PLAY_SCRIPT" >&2; exit 1; }
command -v node    >/dev/null 2>&1 || { echo "error: node not found" >&2; exit 1; }
command -v ffmpeg  >/dev/null 2>&1 || { echo "error: ffmpeg not found" >&2; exit 1; }
command -v cargo   >/dev/null 2>&1 || { echo "error: cargo not found" >&2; exit 1; }

cd "$REPO_ROOT"

if [[ ! -x "$REPO_ROOT/target/release/harnx" ]] || [[ ! -x "$REPO_ROOT/target/release/harnx-mock-llm" ]]; then
  echo "Building release binaries..."
  cargo build --release -p harnx -p harnx-test-bins
fi

(
  cd "$DEMOS_DIR/web"
  if [[ ! -d node_modules ]]; then
    echo "Installing Playwright (one-time)..."
    npm install --silent
    npx --yes playwright install chromium
  fi
)

export PATH="$REPO_ROOT/target/release:$PATH"
export HARNX_CONFIG_DIR="$DEMOS_DIR/config"
export HARNX_SERVE_URL="http://127.0.0.1:$SERVE_PORT"

mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.webm 2>/dev/null || true

MOCK_LOG="$(mktemp -t harnx-mock-llm.XXXXXX.log)"
SERVE_LOG="$(mktemp -t harnx-serve.XXXXXX.log)"
MOCK_PID=""
SERVE_PID=""

cleanup() {
  for pid in "$SERVE_PID" "$MOCK_PID"; do
    [[ -n "$pid" ]] || continue
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -f "$MOCK_LOG" "$SERVE_LOG"
}
trap cleanup EXIT INT TERM

harnx-mock-llm --port "$MOCK_PORT" --script "$SCRIPT_PATH" >"$MOCK_LOG" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 30); do
  grep -q "^READY" "$MOCK_LOG" 2>/dev/null && break
  sleep 0.1
done
grep -q "^READY" "$MOCK_LOG" 2>/dev/null || { echo "error: mock-llm did not start" >&2; cat "$MOCK_LOG" >&2; exit 1; }

harnx --serve "127.0.0.1:$SERVE_PORT" >"$SERVE_LOG" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$SERVE_PORT/v1/models" >/dev/null 2>&1 && break
  sleep 0.2
done
if ! curl -sf "http://127.0.0.1:$SERVE_PORT/v1/models" >/dev/null 2>&1; then
  echo "error: harnx --serve did not come up" >&2
  cat "$SERVE_LOG" >&2
  exit 1
fi

echo "Recording $DEMO with Playwright..."
( cd "$DEMOS_DIR/web" && node "$PLAY_SCRIPT" --out "$OUT_DIR" )

WEBM="$(find "$OUT_DIR" -maxdepth 1 -name "*.webm" -type f | head -n1)"
[[ -f "$WEBM" ]] || { echo "error: no .webm produced by Playwright" >&2; exit 1; }

echo "Converting to GIF..."
ffmpeg -y -loglevel error \
  -i "$WEBM" \
  -vf "fps=15,scale=1280:-1:flags=lanczos,split[s0][s1];[s0]palettegen=stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=5" \
  "$FINAL_GIF"

echo "Done. Output: $FINAL_GIF"
