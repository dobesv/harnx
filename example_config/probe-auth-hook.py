#!/usr/bin/env python3
"""Probe a harnx-proxy-auth exec hook (e.g. jira-auth-hook.py) directly.

Spawns the hook as a subprocess — inheriting the current environment, so it
reads the same acli config / keyring / HARNX_* vars the real proxy would — then
sends it transform requests for the debug endpoint plus a few hosts, and prints
the responses. Any injected Authorization header is decoded with the token
masked, so you can see the email + token prefix + length the hook would inject
without leaking the secret. The hook's own stderr/log lines are shown prefixed
with `┆`.

Usage:
  probe-auth-hook.py [HOOK_PATH] [HOST ...]

Defaults:
  HOOK_PATH = ~/.config/harnx/packages/pantheon/hooks/jira-auth-hook.py
  HOSTS     = as.atlassian.com api.atlassian.com
"""
import base64
import json
import os
import subprocess
import sys
import tempfile
import threading

DEFAULT_HOOK = os.path.expanduser(
    "~/.config/harnx/packages/pantheon/hooks/jira-auth-hook.py"
)


def mask_auth(value):
    if not value:
        return "(none)"
    scheme, _, rest = value.partition(" ")
    if scheme == "Basic":
        try:
            decoded = base64.b64decode(rest).decode("utf-8", "replace")
            user, _, tok = decoded.partition(":")
            return f"Basic {user}:{tok[:8]}…({len(tok)} chars)"
        except Exception:
            return "Basic (undecodable)"
    if scheme == "Bearer":
        return f"Bearer {rest[:8]}…({len(rest)} chars)"
    return f"{scheme} …"


def main():
    hook = DEFAULT_HOOK
    hosts = []
    for arg in sys.argv[1:]:
        if arg.endswith(".py") or "/" in arg:
            hook = os.path.expanduser(arg)
        else:
            hosts.append(arg)
    if not hosts:
        hosts = ["as.atlassian.com", "api.atlassian.com"]

    print(f"probing hook: {hook}\n(hook stderr shown with ┆)\n")
    proc = subprocess.Popen(
        [sys.executable, hook],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    def pump_stderr():
        for line in proc.stderr:
            sys.stderr.write("  ┆ " + line)

    threading.Thread(target=pump_stderr, daemon=True).start()

    startup_root = tempfile.mkdtemp(prefix="probe-auth-hook-", dir="/tmp")
    startup_request = {
        "id": "probe-startup",
        "event": "startup",
        "vars": {
            "temp_file_root": startup_root,
            "proxy_port": 4444,
        },
    }
    requests = [
        {
            "id": "debug",
            "host": "harnx.invalid",
            "method": "GET",
            "path": "/jira-auth-hook/debug",
            "headers": {},
        }
    ]
    for i, host in enumerate(hosts):
        requests.append(
            {
                "id": str(i + 1),
                "host": host,
                "method": "POST",
                "path": "/api/v1/batch",
                # A dummy auth the hook is expected to overwrite:
                "headers": {"authorization": "Basic c2VudGluZWw6c2VudGluZWw="},
            }
        )

    proc.stdin.write(json.dumps(startup_request) + "\n")
    proc.stdin.flush()

    pending = {r["id"]: r for r in requests}
    for r in requests:
        proc.stdin.write(json.dumps(r) + "\n")
        proc.stdin.flush()

    got = 0
    startup_seen = False
    while not startup_seen or got < len(requests):
        line = proc.stdout.readline()
        if not line:
            break
        line = line.strip()
        if not line or line == "READY":
            continue
        try:
            resp = json.loads(line)
        except Exception:
            print(f"  (non-JSON stdout) {line}")
            continue
        if "notice" in resp and "id" not in resp:
            notice = resp["notice"]
            print(f"  ⚑ NOTICE [{notice.get('level')}] {notice.get('message')}")
            continue
        rid = resp.get("id")
        if rid == "probe-startup":
            startup_seen = True
            print(f"[startup] temp_file_root={startup_root}")
            print(f"[startup] env={json.dumps(resp.get('env') or {}, sort_keys=True)}")
            continue
        req = pending.get(rid)
        got += 1
        if rid == "debug":
            print(f"[debug] {resp.get('respond', {}).get('body')}")
            continue
        host = req["host"] if req else "?"
        injected = resp.get("headers", {}).get("authorization")
        if injected:
            print(f"[{host}] injects → {mask_auth(injected)}")
        else:
            print(f"[{host}] no injection (request auth passed through unchanged)")

    proc.stdin.close()
    try:
        proc.wait(timeout=3)
    except Exception:
        proc.kill()


if __name__ == "__main__":
    main()
