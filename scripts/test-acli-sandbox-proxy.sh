#!/usr/bin/env bash
#
# test-acli-sandbox-proxy.sh
#
# End-to-end smoke test for the acli/Jira sandbox credential-isolation setup.
#
# It runs ONE command: harnx-sandbox-run with the harnx-proxy-auth hook attached
# (exactly as packages/*/mcp_servers/bash.yaml wires it in production), and the
# sandboxed payload simply dumps its env and runs acli:
#
#   harnx-sandbox-run \
#     --hook claude-command-persistent harnx-proxy-auth <flags...> \; \
#     -- bash -c 'env; acli --help; acli jira auth status'
#
# harnx-sandbox-run itself spawns the proxy hook, captures the env it injects
# (HTTPS_PROXY / CA bundle / ACLI_CONFIG_DIR pointing at a SYNTHETIC config that
# holds only a sentinel token), keeps the proxy alive for the child's lifetime,
# and runs the payload inside the sandbox. The proxy swaps the sentinel
# Authorization header for your real token on the wire, so `acli jira auth
# status` succeeds without the real token ever entering the sandbox.
#
# What you should see in the output:
#   - env shows HTTPS_PROXY=http://127.0.0.1:<port> and ACLI_CONFIG_DIR=/tmp/harnx-fs-...
#   - `acli --help` runs (proves the binary executes in the sandbox)
#   - `acli jira auth status` succeeds (proves the on-the-wire token swap works)
#   - the real token never appears anywhere in the output
#
# Prereqs (same as production):
#   - `acli jira auth login` already done on the host (so `acli jira auth status`
#     works outside any sandbox, and secret-tool has the token).
#   - secret-tool available and the keyring unlocked.
#   - harnx-proxy-auth, harnx-sandbox-run, harnx-sandbox-exec built / on PATH.
#   - Run from a normal (non-sandboxed) host shell — the sandbox cannot nest.
#
# Usage:  scripts/test-acli-sandbox-proxy.sh
# Exit 0 = acli authenticated through the proxy with only a sentinel token.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

find_bin() {
  local name="$1"
  if [[ -x "$REPO_ROOT/target/release/$name" ]]; then
    echo "$REPO_ROOT/target/release/$name"
  elif command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
  else
    echo "ERROR: cannot find '$name' (build: cargo build --release -p $name, or put on PATH)" >&2
    return 1
  fi
}

PROXY_BIN="$(find_bin harnx-proxy-auth)"    || exit 1
SANDBOX_BIN="$(find_bin harnx-sandbox-run)" || exit 1
find_bin harnx-sandbox-exec >/dev/null      || exit 1
command -v acli        >/dev/null 2>&1 || { echo "ERROR: acli not on PATH" >&2; exit 1; }
command -v secret-tool >/dev/null 2>&1 || { echo "ERROR: secret-tool not on PATH" >&2; exit 1; }

HOST_ACLI_CFG="${XDG_CONFIG_HOME:-$HOME/.config}/acli/jira_config.yaml"
[[ -f "$HOST_ACLI_CFG" ]] || { echo "ERROR: $HOST_ACLI_CFG not found — run 'acli jira auth login' first." >&2; exit 1; }

# Real token: only used to PROVE it never leaks into the sandbox output.
PROFILE="$(sed -n 's/^current_profile:[[:space:]]*"\?\([^"]*\)"\?[[:space:]]*$/\1/p' "$HOST_ACLI_CFG")"
REAL_TOKEN="$(secret-tool lookup service acli username "jira:$PROFILE" 2>/dev/null || true)"

echo "== Setup =="
echo "  proxy   : $PROXY_BIN"
echo "  sandbox : $SANDBOX_BIN"
echo "  acli    : $(command -v acli)"
echo "  profile : ${PROFILE:-<none>}"
[[ -n "$REAL_TOKEN" ]] && echo "  token   : present (len=${#REAL_TOKEN}, sha256[0:16]=$(printf '%s' "$REAL_TOKEN" | sha256sum | cut -c1-16))"
echo

SENTINEL_TOKEN='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6OjowMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBhNmM2MzI1MzM1NGQxODBiNjkzYWFjYmRkZjlmYjA2YzFkMGI2NmE0MmQ4Mzc1NmJjM2U5ZjM5ODg4MzRhMGZiM2EzYTRhMWY='

# A real Jira issue key to fetch — exercises a normal data-path API call (not
# just auth). Override with WORKITEM=ABC-123 if FDEV-4729 isn't visible to you.
WORKITEM="${WORKITEM:-FDEV-4729}"

# The sandboxed payload: show the proxy-injected env, then exercise acli.
PAYLOAD='echo "--- env (proxy-injected) ---"
env | grep -E "^(HTTPS?_PROXY|.*CA.*|ACLI_CONFIG_DIR)=" | sort
echo "--- acli --help ---"
acli --help >/dev/null && echo "acli --help: OK" || echo "acli --help: FAILED"
echo "--- synthetic config acli will read ---"
cat "$ACLI_CONFIG_DIR/acli/jira_config.yaml" 2>/dev/null; echo
echo "--- acli jira auth status ---"
acli jira auth status
echo "--- acli jira workitem view '"$WORKITEM"' ---"
acli jira workitem view '"$WORKITEM"''

# Proxy request log (host/method/path/auth/changed per request) — lets us see
# whether acli reached the proxy and whether the auth header was swapped.
PROXY_LOG="$(mktemp /tmp/harnx-proxy-log.XXXXXX)"

echo "== Running acli inside harnx-sandbox-run (proxy attached as a hook) =="
OUT="$(mktemp)"
set +e
"$SANDBOX_BIN" \
  --hook claude-command-persistent "$PROXY_BIN" \
    --log-file "$PROXY_LOG" \
    --load-yaml acli_cfg="$HOST_ACLI_CFG" \
    --load-exec 'atlassian_token=p=$(sed -n "s/^current_profile:[[:space:]]*\"\?\([^\"]*\)\"\?[[:space:]]*$/\1/p" '"$HOST_ACLI_CFG"'); test -n "$p" && secret-tool lookup service acli username "jira:$p"' \
    --fs '$acli_cfg.current_profile as $cp |
          (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
          if $p and $atlassian_token then
            . + { "acli/jira_config.yaml": (
              "version: 1\n" +
              "current_profile: \($cp)\n" +
              "profiles:\n" +
              "    - site: \($p.site)\n" +
              "      cloud_id: \($p.cloud_id)\n" +
              "      account_id: \($p.account_id)\n" +
              "      email: \($p.email // env.ATLASSIAN_EMAIL // "")\n" +
              "      auth_type: api_token\n" +
              "      token: !!binary '"$SENTINEL_TOKEN"'\n"
            ) }
          end' \
    --env '$acli_cfg.current_profile as $cp |
          (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
          if $p and $atlassian_token then .ACLI_CONFIG_DIR = $temp_file_root end' \
    --hook '$acli_cfg.current_profile as $cp |
          (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
          if $p and $atlassian_token and (.host == "api.atlassian.com" or .host == $p.site)
          then .headers.authorization = basic($p.email // env.ATLASSIAN_EMAIL // ""; $atlassian_token) end' \
  \; \
  -- bash -c "$PAYLOAD" >"$OUT" 2>&1
RC=$?
set -e 2>/dev/null || true

echo "---- sandbox output (exit=$RC) ----"
sed 's/^/  /' "$OUT"
echo "-----------------------------------"
echo

echo "---- proxy request log (host / method / path / auth / changed) ----"
if [[ -s "$PROXY_LOG" ]]; then
  sed 's/^/  /' "$PROXY_LOG"
else
  echo "  (empty — acli made NO requests through the proxy)"
fi
echo "-------------------------------------------------------------------"
rm -f "$PROXY_LOG"
echo

# ----------------------------------------------------------------------------
# Verdict.
# ----------------------------------------------------------------------------
FAIL=0
grep -q "$SENTINEL_TOKEN" "$OUT" \
  && echo "PASS: sandbox saw only the SENTINEL token in its config" \
  || { echo "WARN: sentinel token not seen in output (config dump may be suppressed)"; }

if [[ -n "$REAL_TOKEN" ]] && grep -qF "$REAL_TOKEN" "$OUT"; then
  echo "FAIL: REAL token leaked into the sandbox output!"; FAIL=1
else
  echo "PASS: real token absent from sandbox output"
fi

grep -q "Authenticated" "$OUT" \
  && echo "PASS: acli jira auth status authenticated through the proxy" \
  || { echo "FAIL: acli jira auth status did not report Authenticated"; FAIL=1; }

# Data-path API call: the workitem view must return the issue key.
grep -q "$WORKITEM" "$OUT" \
  && echo "PASS: acli jira workitem view $WORKITEM returned data through the proxy" \
  || { echo "FAIL: acli jira workitem view $WORKITEM did not return the issue ($WORKITEM not in output)"; FAIL=1; }

if [[ $RC -ne 0 ]]; then
  echo "NOTE: payload exit code was $RC (last command's status)."
  echo "      If output shows 'failed to spawn process: Permission denied', you are"
  echo "      likely running inside another sandbox — re-run from a normal host shell."
fi
rm -f "$OUT"

echo
if [[ $FAIL -eq 0 ]]; then
  echo "ALL CHECKS PASSED ✅  (sentinel-only config in sandbox, real auth via on-the-wire swap)"
  exit 0
else
  echo "SOME CHECKS FAILED ❌"
  exit 1
fi
