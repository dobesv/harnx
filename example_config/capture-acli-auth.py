#!/usr/bin/env python3
"""Capture which Authorization header authenticated `acli` sends to each host.

This is a diagnostic for the harnx-proxy-auth jira-auth-hook. It runs a local
MITM proxy in-process, points `acli` at it (already-authenticated on the host),
and prints — per host + path — the auth scheme acli actually sends. Tokens are
masked; only the scheme + a short prefix + length are shown, so nothing secret
is leaked.

Use it to answer: for `acli jira auth status` / `workitem view`, which host(s)
does acli contact and what Authorization does it send there? That tells us which
hosts the hook must inject auth for (e.g. as.atlassian.com/api/v1/batch vs
api.atlassian.com/cli/...).

Requires mitmproxy: `pip install mitmproxy` (or `pipx run --spec mitmproxy ...`).

Usage:
  ./capture-acli-auth.py                       # runs the default acli commands
  ./capture-acli-auth.py 'acli jira auth status' 'acli jira project list'
  ./capture-acli-auth.py --port 8899 'acli jira workitem view FDEV-4895'

Each positional arg is a full shell command run through the proxy. If none are
given, a sensible default set of acli commands is used.
"""
import argparse
import asyncio
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

DEFAULT_COMMANDS = [
    "acli jira auth status",
    "acli jira workitem view FDEV-4895",
]

# Only report hosts we care about; everything else (telemetry, sentry, github)
# is noise for this diagnostic.
INTERESTING_SUBSTRINGS = ("atlassian.com", "atlassian.net")


def mask_auth(value):
    """Return a redacted, safe-to-print form of an Authorization header value."""
    if not value:
        return "<none>"
    parts = value.split(" ", 1)
    scheme = parts[0]
    rest = parts[1] if len(parts) > 1 else ""
    if not rest:
        return scheme
    return f"{scheme} <{len(rest)} chars, starts {rest[:6]!r}>"


def check_mitmproxy():
    try:
        import mitmproxy  # noqa: F401
    except ImportError:
        sys.exit(
            "mitmproxy is not installed.\n"
            "  Install it with:  pip install mitmproxy\n"
            "  Or run once with: pipx run --spec mitmproxy python3 "
            + __file__
        )


class AuthRecorder:
    """mitmproxy addon: record (method, host, path, auth) per request."""

    def __init__(self):
        self.rows = []

    def request(self, flow):
        host = flow.request.host
        if not any(s in host for s in INTERESTING_SUBSTRINGS):
            return
        auth = flow.request.headers.get("authorization")
        self.rows.append(
            (flow.request.method, host, flow.request.path, mask_auth(auth))
        )


def run_proxy(recorder, port, confdir, ready_event, stop_event):
    """Run a mitmproxy DumpMaster in this thread until stop_event is set."""
    import asyncio as _asyncio
    from mitmproxy import options
    from mitmproxy.tools import dump

    loop = _asyncio.new_event_loop()
    _asyncio.set_event_loop(loop)

    async def _serve():
        opts = options.Options(
            listen_host="127.0.0.1",
            listen_port=port,
            confdir=str(confdir),
        )
        master = dump.DumpMaster(opts, with_termlog=False, with_dumper=False)
        master.addons.add(recorder)
        # Signal readiness once the CA cert exists on disk.
        async def _watch_ready():
            ca = Path(confdir) / "mitmproxy-ca-cert.pem"
            for _ in range(200):
                if ca.exists():
                    ready_event.set()
                    return
                await _asyncio.sleep(0.05)
            ready_event.set()  # proceed anyway; cert may still appear
        _asyncio.ensure_future(_watch_ready())

        async def _watch_stop():
            while not stop_event.is_set():
                await _asyncio.sleep(0.1)
            master.shutdown()
        _asyncio.ensure_future(_watch_stop())

        await master.run()

    try:
        loop.run_until_complete(_serve())
    except Exception as exc:  # pragma: no cover - diagnostic best-effort
        print(f"[proxy] stopped: {exc}", file=sys.stderr)
    finally:
        loop.close()


def run_command(cmd, port, ca_cert):
    """Run one shell command with proxy + CA env vars pointed at our MITM."""
    env = dict(os.environ)
    proxy_url = f"http://127.0.0.1:{port}"
    env["HTTP_PROXY"] = proxy_url
    env["HTTPS_PROXY"] = proxy_url
    env["http_proxy"] = proxy_url
    env["https_proxy"] = proxy_url
    # Cover the common CA-bundle env vars different runtimes honor, so acli
    # trusts the MITM cert regardless of what HTTP stack it uses.
    env["SSL_CERT_FILE"] = str(ca_cert)
    env["REQUESTS_CA_BUNDLE"] = str(ca_cert)
    env["NODE_EXTRA_CA_CERTS"] = str(ca_cert)
    env["CURL_CA_BUNDLE"] = str(ca_cert)
    env["GIT_SSL_CAINFO"] = str(ca_cert)

    print(f"\n$ {cmd}")
    try:
        proc = subprocess.run(
            cmd, shell=True, env=env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            timeout=60,
        )
        out = proc.stdout.strip()
        if out:
            for line in out.splitlines():
                print(f"  | {line}")
        print(f"  (exit {proc.returncode})")
    except subprocess.TimeoutExpired:
        print("  (timed out after 60s)")
    except Exception as exc:
        print(f"  (failed to run: {exc})")


def main():
    parser = argparse.ArgumentParser(
        description="Capture acli's per-host Authorization headers via MITM."
    )
    parser.add_argument("--port", type=int, default=8888, help="MITM listen port")
    parser.add_argument(
        "commands", nargs="*",
        help="Shell commands to run through the proxy (default: acli auth+view)",
    )
    args = parser.parse_args()
    commands = args.commands or DEFAULT_COMMANDS

    check_mitmproxy()

    if not shutil.which("acli") and any("acli" in c for c in commands):
        print(
            "WARNING: `acli` not found on PATH. The commands will likely fail.\n"
            "Run this on the machine where `acli jira auth status` works.\n",
            file=sys.stderr,
        )

    confdir = Path(tempfile.mkdtemp(prefix="acli-capture-mitm-"))
    ca_cert = confdir / "mitmproxy-ca-cert.pem"
    recorder = AuthRecorder()
    ready = threading.Event()
    stop = threading.Event()

    proxy_thread = threading.Thread(
        target=run_proxy,
        args=(recorder, args.port, confdir, ready, stop),
        daemon=True,
    )
    proxy_thread.start()

    print(f"Starting MITM proxy on 127.0.0.1:{args.port} (confdir {confdir}) ...")
    if not ready.wait(timeout=15):
        stop.set()
        sys.exit("MITM proxy failed to start / generate a CA cert in time.")
    print(f"CA cert: {ca_cert}")
    print("acli will be run with HTTPS_PROXY + CA env vars pointed at the proxy.")

    try:
        for cmd in commands:
            run_command(cmd, args.port, ca_cert)
    finally:
        # Give in-flight requests a moment to be recorded, then stop the proxy.
        time.sleep(0.5)
        stop.set()
        proxy_thread.join(timeout=5)
        shutil.rmtree(confdir, ignore_errors=True)

    print("\n" + "=" * 72)
    print("CAPTURED ATLASSIAN REQUESTS (auth masked):")
    print("=" * 72)
    if not recorder.rows:
        print(
            "No Atlassian requests captured. Possible causes:\n"
            "  - acli did not trust the MITM cert (check for TLS errors above)\n"
            "  - acli uses a proxy-bypass / different network path\n"
            "  - the command failed before making network calls"
        )
    else:
        seen = set()
        for method, host, path, auth in recorder.rows:
            # Collapse duplicate (method, host, path, auth) rows.
            key = (method, host, path, auth)
            if key in seen:
                continue
            seen.add(key)
            print(f"  {method:7} https://{host}{path}")
            print(f"          AUTH: {auth}")

    print(
        "\nWhat to look for: the host+path that carries a real Basic/Bearer token\n"
        "is the one the hook must inject auth for. Paste this block back to the\n"
        "agent so the jira-auth-hook TARGET_HOSTS / injection rule can be fixed."
    )


if __name__ == "__main__":
    main()
