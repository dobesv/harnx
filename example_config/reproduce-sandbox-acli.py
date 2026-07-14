#!/usr/bin/env python3
"""Reproduce the sandbox acli auth setup on the host, to find why it fails.

The real proxy flow is: the jira-auth-hook writes a SYNTHETIC jira_config.yaml
(with a *sentinel* token) into $ACLI_CONFIG_DIR, and harnx-proxy-auth swaps the
sentinel/blank Authorization for the real token on api.atlassian.com. In the
sandbox, acli reads that synthetic config and then makes its network calls.

This script reproduces just the *config* half on the host so we can see whether
acli even accepts the synthetic config and proceeds to api.atlassian.com — the
step that was missing in the sandbox log. It:

  1. Shows the shape of your REAL host config (token line redacted) so we can
     compare formats (plaintext vs SecretStore AES-GCM blob).
  2. Builds a synthetic config identical to what the hook writes, under a temp
     ACLI_CONFIG_DIR, at BOTH candidate layouts:
       A) $ACLI_CONFIG_DIR/acli/jira_config.yaml   (what the hook does now)
       B) $ACLI_CONFIG_DIR/jira_config.yaml         (alternative layout)
  3. Runs `acli jira auth status` with ACLI_CONFIG_DIR pointed at each layout,
     through the capture MITM proxy, and reports which requests acli makes and
     the status/body — so we can see whether acli reaches api.atlassian.com.

Nothing secret is printed: the real token is never read here (the synthetic
config uses the sentinel), and the host config's token line is redacted.

Usage:
  ./reproduce-sandbox-acli.py            # uses ~/.config/acli/jira_config.yaml
  ./reproduce-sandbox-acli.py /path/to/jira_config.yaml
  ./reproduce-sandbox-acli.py --port 8899
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

# Reuse the MITM capture machinery from the sibling script.
sys.path.insert(0, str(Path(__file__).resolve().parent))
try:
    from capture_acli_auth import AuthRecorder, run_proxy, check_mitmproxy  # type: ignore
except Exception:
    # capture-acli-auth.py has a hyphen; import it by file path instead.
    import importlib.util

    _spec = importlib.util.spec_from_file_location(
        "capture_acli_auth",
        str(Path(__file__).resolve().parent / "capture-acli-auth.py"),
    )
    _mod = importlib.util.module_from_spec(_spec)
    _spec.loader.exec_module(_mod)
    AuthRecorder = _mod.AuthRecorder
    run_proxy = _mod.run_proxy
    check_mitmproxy = _mod.check_mitmproxy

DEFAULT_HOST_CONFIG = "~/.config/acli/jira_config.yaml"


def redact_config(text):
    """Return the config text with any token/secret line value redacted."""
    out = []
    for line in text.splitlines():
        stripped = line.strip()
        low = stripped.lower()
        if low.startswith(("token:", "api_token:", "secret:", "password:")):
            key = line.split(":", 1)[0]
            val = line.split(":", 1)[1].strip()
            out.append(f"{key}: <redacted, {len(val)} chars, "
                       f"looks like {'SecretStore-blob' if ':::' in val else 'plaintext'}>")
        else:
            out.append(line)
    return "\n".join(out)


def run_acli_through_proxy(config_dir, port, ca_cert, label):
    env = dict(os.environ)
    proxy_url = f"http://127.0.0.1:{port}"
    for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"):
        env[k] = proxy_url
    for k in ("SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "NODE_EXTRA_CA_CERTS",
              "CURL_CA_BUNDLE", "GIT_SSL_CAINFO"):
        env[k] = str(ca_cert)
    env["ACLI_CONFIG_DIR"] = str(config_dir)

    print(f"\n--- {label} ---")
    print(f"ACLI_CONFIG_DIR={config_dir}")
    try:
        proc = subprocess.run(
            "acli jira auth status", shell=True, env=env,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60,
        )
        for line in (proc.stdout or "").strip().splitlines():
            print(f"  | {line}")
        print(f"  (exit {proc.returncode})")
    except Exception as exc:
        print(f"  (failed: {exc})")


def load_hook():
    """Import the jira-auth-hook module (hyphenated filename) by path."""
    import importlib.util

    hook_path = Path(__file__).resolve().parent / "jira-auth-hook.py"
    spec = importlib.util.spec_from_file_location("jira_auth_hook", str(hook_path))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def build_synthetic(base_dir, hook, host_config_text, nested):
    """Write the SENTINEL-token synthetic config exactly like the hook does.

    This is the crux: the sandbox acli reads a config whose token is the hook's
    SENTINEL blob (not the real token — the proxy swaps auth on the wire). We
    reproduce that here to see whether acli ACCEPTS the sentinel config locally
    and proceeds to api.atlassian.com. If acli rejects the sentinel token before
    the network (e.g. it wants a valid SecretStore blob), it never reaches the
    endpoint the proxy would authenticate.

    nested=True  -> base_dir/acli/jira_config.yaml  (hook's current layout)
    nested=False -> base_dir/jira_config.yaml
    """
    # Parse the host config to reuse its profile fields (site/cloud_id/etc).
    current_profile, profiles = hook._parse_host_config_text(host_config_text)
    profile = profiles[0] if profiles else {}
    target = base_dir / "acli" if nested else base_dir
    target.mkdir(parents=True, exist_ok=True)
    email = profile.get("email") or os.environ.get("ATLASSIAN_EMAIL", "")
    config_text = "\n".join([
        "version: 1",
        f"current_profile: {current_profile}",
        "profiles:",
        f"    - site: {profile.get('site', '')}",
        f"      cloud_id: {profile.get('cloud_id', '')}",
        f"      account_id: {profile.get('account_id', '')}",
        f"      email: {email}",
        "      auth_type: api_token",
        f"      token: {hook.SENTINEL_TOKEN_BLOB}",
        "",
    ])
    (target / "jira_config.yaml").write_text(config_text, encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host_config", nargs="?", default=DEFAULT_HOST_CONFIG)
    parser.add_argument("--port", type=int, default=8890)
    args = parser.parse_args()

    check_mitmproxy()
    if not shutil.which("acli"):
        sys.exit("`acli` not found on PATH. Run on the machine where acli works.")

    host_config = Path(args.host_config).expanduser()
    if not host_config.exists():
        sys.exit(f"host config not found: {host_config}")
    host_text = host_config.read_text(encoding="utf-8")
    hook = load_hook()

    print("=" * 72)
    print(f"HOST CONFIG: {host_config}  (token redacted)")
    print("=" * 72)
    print(redact_config(host_text))

    # Start MITM proxy.
    confdir = Path(tempfile.mkdtemp(prefix="repro-acli-mitm-"))
    ca_cert = confdir / "mitmproxy-ca-cert.pem"
    recorder = AuthRecorder()
    ready, stop = threading.Event(), threading.Event()
    t = threading.Thread(target=run_proxy,
                         args=(recorder, args.port, confdir, ready, stop),
                         daemon=True)
    t.start()
    if not ready.wait(timeout=15):
        stop.set()
        sys.exit("MITM proxy failed to start.")

    # Layout A: SENTINEL token, nested acli/ subdir (exactly what the hook writes).
    dir_a = Path(tempfile.mkdtemp(prefix="repro-acli-A-"))
    build_synthetic(dir_a, hook, host_text, nested=True)
    run_acli_through_proxy(dir_a, args.port, ca_cert,
                           "A) SENTINEL token, $ACLI_CONFIG_DIR/acli/jira_config.yaml (hook's exact output)")

    # Layout B: SENTINEL token, file directly under ACLI_CONFIG_DIR.
    dir_b = Path(tempfile.mkdtemp(prefix="repro-acli-B-"))
    build_synthetic(dir_b, hook, host_text, nested=False)
    run_acli_through_proxy(dir_b, args.port, ca_cert,
                           "B) SENTINEL token, $ACLI_CONFIG_DIR/jira_config.yaml (alt layout)")

    # Control C: REAL host config copied verbatim, nested layout. If this reaches
    # api.atlassian.com but A/B do not, the SENTINEL TOKEN is the problem (acli
    # rejects it locally). If C also fails, the LAYOUT is wrong.
    dir_c = Path(tempfile.mkdtemp(prefix="repro-acli-C-"))
    (dir_c / "acli").mkdir(parents=True, exist_ok=True)
    (dir_c / "acli" / "jira_config.yaml").write_text(host_text, encoding="utf-8")
    run_acli_through_proxy(dir_c, args.port, ca_cert,
                           "C) CONTROL: real host config, $ACLI_CONFIG_DIR/acli/jira_config.yaml")

    time.sleep(0.5)
    stop.set()
    t.join(timeout=5)
    for d in (confdir, dir_a, dir_b, dir_c):
        shutil.rmtree(d, ignore_errors=True)

    print("\n" + "=" * 72)
    print("REQUESTS acli MADE (both layouts combined; auth masked):")
    print("=" * 72)
    if not recorder.rows:
        print("No Atlassian requests captured under either layout.")
    else:
        seen = set()
        for row in recorder.rows:
            key = (row["method"], row["host"], row["path"], row["status"])
            if key in seen:
                continue
            seen.add(key)
            print(f"  {row['method']:5} https://{row['host']}{row['path'][:70]}")
            print(f"        AUTH {row['auth']}  STATUS {row['status']}")

    print(
        "\nInterpretation:\n"
        "  - If a layout reaches api.atlassian.com -> that ACLI_CONFIG_DIR layout\n"
        "    is correct; acli accepted the config and proceeded.\n"
        "  - If NEITHER reaches api.atlassian.com (only as.atlassian.com) and acli\n"
        "    says unauthorized -> acli did not accept the config (layout wrong OR\n"
        "    the token format in the config is rejected locally). Compare with the\n"
        "    host-config token format printed above (plaintext vs SecretStore blob).\n"
        "Paste this whole block back to the agent."
    )


if __name__ == "__main__":
    main()
