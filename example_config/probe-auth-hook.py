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


def parse_args(argv):
    """Return (hook_path, [host, ...]) from CLI args."""
    hook = DEFAULT_HOOK
    hosts = []
    for arg in argv:
        if arg.endswith(".py") or "/" in arg:
            hook = os.path.expanduser(arg)
        else:
            hosts.append(arg)
    if not hosts:
        hosts = ["as.atlassian.com", "api.atlassian.com"]
    return hook, hosts


def pump_stderr(proc):
    for line in proc.stderr:
        sys.stderr.write("  ┆ " + line)


def build_requests(hosts):
    """Return the debug + per-host transform request messages."""
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
    return requests


def send_line(stdin, payload):
    stdin.write(json.dumps(payload) + "\n")
    stdin.flush()


def print_startup_response(resp, startup_root):
    print(f"[startup] temp_file_root={startup_root}")
    print(f"[startup] env={json.dumps(resp.get('env') or {}, sort_keys=True)}")


def print_request_response(resp, pending):
    rid = resp.get("id")
    if rid == "debug":
        print(f"[debug] {resp.get('respond', {}).get('body')}")
        return
    req = pending.get(rid)
    host = req["host"] if req else "?"
    injected = resp.get("headers", {}).get("authorization")
    if injected:
        print(f"[{host}] injects → {mask_auth(injected)}")
    else:
        print(f"[{host}] no injection (request auth passed through unchanged)")


def _parse_response_line(line):
    """Return the decoded response object for a stdout line, or None to skip."""
    line = line.strip()
    if not line or line == "READY":
        return None
    try:
        return json.loads(line)
    except Exception:
        print(f"  (non-JSON stdout) {line}")
        return None


def read_responses(proc, pending, request_count, startup_root):
    """Read hook responses until the startup reply and all requests are seen."""
    got = 0
    startup_seen = False
    while not startup_seen or got < request_count:
        line = proc.stdout.readline()
        if not line:
            break
        resp = _parse_response_line(line)
        if resp is None:
            continue
        if "notice" in resp and "id" not in resp:
            notice = resp["notice"]
            print(f"  ⚑ NOTICE [{notice.get('level')}] {notice.get('message')}")
        elif resp.get("id") == "probe-startup":
            startup_seen = True
            print_startup_response(resp, startup_root)
        else:
            got += 1
            print_request_response(resp, pending)


def main():
    hook, hosts = parse_args(sys.argv[1:])

    print(f"probing hook: {hook}\n(hook stderr shown with ┆)\n")
    proc = subprocess.Popen(
        [sys.executable, hook],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    threading.Thread(target=pump_stderr, args=(proc,), daemon=True).start()

    startup_root = tempfile.mkdtemp(prefix="probe-auth-hook-", dir="/tmp")
    startup_request = {
        "id": "probe-startup",
        "event": "startup",
        "vars": {
            "temp_file_root": startup_root,
            "proxy_port": 4444,
        },
    }
    requests = build_requests(hosts)

    send_line(proc.stdin, startup_request)
    pending = {r["id"]: r for r in requests}
    for r in requests:
        send_line(proc.stdin, r)

    read_responses(proc, pending, len(requests), startup_root)

    proc.stdin.close()
    try:
        proc.wait(timeout=3)
    except Exception:
        proc.kill()


if __name__ == "__main__":
    main()
