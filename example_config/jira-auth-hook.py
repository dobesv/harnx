#!/usr/bin/env python3
import base64
import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

SENTINEL_TOKEN_BLOB = (
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6OjowMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBh"
    "NmM2MzI1MzM1NGQxODBiNjkzYWFjYmRkZjlmYjA2YzFkMGI2NmE0MmQ4Mzc1NmJjM2U5ZjM5ODg4MzRh"
    "MGZiM2EzYTRhMWY="
)
TOKEN_CMD_ENV = "HARNX_JIRA_TOKEN_CMD"
DEFAULT_TOKEN_CMD = 'secret-tool lookup service acli username {profile_arg}'
HOST_CONFIG_ENV_NAMES = ["ACLI_HOST_CONFIG", "HARNX_JIRA_HOST_CONFIG"]
DEFAULT_HOST_CONFIG = "~/.config/acli/jira_config.yaml"
TEMP_ROOT_ENV_NAMES = ["HARNX_JIRA_TEMP_ROOT", "TEMP_FILE_ROOT", "TMPDIR", "TMP", "TEMP"]
DEFAULT_TEMP_ROOT = "/tmp"

REAL_TOKEN = None
PROFILE = None
TARGET_HOSTS = set()
AUTHORIZATION_HEADER = None
SANDBOX_CONFIG_DIR = None


def log(message):
    print(message, file=sys.stderr, flush=True)


def emit(response):
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()


def parse_scalar(value):
    value = value.strip()
    if not value:
        return ""
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "'"}:
        return value[1:-1]
    return value


def load_host_config(path):
    current_profile = None
    profiles = []
    current = None

    with open(path, "r", encoding="utf-8") as handle:
        for raw_line in handle:
            line = raw_line.rstrip("\n")
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue

            if line.startswith("current_profile:"):
                current_profile = parse_scalar(line.split(":", 1)[1])
                continue

            if line.startswith("profiles:"):
                continue

            if line.startswith("  - "):
                current = {}
                profiles.append(current)
                remainder = line[4:]
                if remainder.strip() and ":" in remainder:
                    key, value = remainder.split(":", 1)
                    current[key.strip()] = parse_scalar(value)
                continue

            if current is not None and line.startswith("    ") and ":" in stripped:
                key, value = stripped.split(":", 1)
                current[key.strip()] = parse_scalar(value)

    if not current_profile:
        raise ValueError("current_profile missing from host config")

    for profile in profiles:
        profile_key = f"{profile.get('cloud_id', '')}:{profile.get('account_id', '')}"
        if profile_key == current_profile:
            return current_profile, profile

    raise ValueError(f"profile matching current_profile not found: {current_profile!r}")


def resolve_host_config_path():
    for env_name in HOST_CONFIG_ENV_NAMES:
        candidate = os.environ.get(env_name)
        if candidate:
            return Path(candidate).expanduser()
    return Path(DEFAULT_HOST_CONFIG).expanduser()


def resolve_temp_root():
    for env_name in TEMP_ROOT_ENV_NAMES:
        candidate = os.environ.get(env_name)
        if candidate:
            return Path(candidate).expanduser()
    return Path(DEFAULT_TEMP_ROOT)


def lookup_token(current_profile):
    token_cmd = os.environ.get(TOKEN_CMD_ENV, DEFAULT_TOKEN_CMD.format(profile_arg=shlex.quote(f"jira:{current_profile}")))
    try:
        completed = subprocess.run(
            token_cmd,
            shell=True,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except Exception as exc:
        raise RuntimeError(f"token command failed: {token_cmd!r}: {exc}") from exc

    token = completed.stdout.strip()
    if not token:
        raise RuntimeError(f"token command produced empty stdout: {token_cmd!r}")
    return token


def write_synthetic_config(current_profile, profile, temp_root):
    config_root = temp_root / "harnx-fs-acli" / "acli"
    config_root.mkdir(parents=True, exist_ok=True)
    config_path = config_root / "jira_config.yaml"
    email = profile.get("email", os.environ.get("ATLASSIAN_EMAIL", ""))
    config_text = "\n".join(
        [
            "version: 1",
            f"current_profile: {current_profile}",
            "profiles:",
            f"    - site: {profile.get('site', '')}",
            f"      cloud_id: {profile.get('cloud_id', '')}",
            f"      account_id: {profile.get('account_id', '')}",
            f"      email: {email}",
            "      auth_type: api_token",
            f"      token: {SENTINEL_TOKEN_BLOB}",
            "",
        ]
    )
    config_path.write_text(config_text, encoding="utf-8")
    return email, config_root.parent


def initialize():
    global REAL_TOKEN, PROFILE, TARGET_HOSTS, AUTHORIZATION_HEADER, SANDBOX_CONFIG_DIR

    host_config = resolve_host_config_path()
    current_profile, profile = load_host_config(host_config)
    real_token = lookup_token(current_profile)
    email, sandbox_config_dir = write_synthetic_config(current_profile, profile, resolve_temp_root())
    target_hosts = {"api.atlassian.com"}
    site = (profile.get("site") or "").strip()
    if site:
        target_hosts.add(site)
    auth = "Basic " + base64.b64encode(f"{email}:{real_token}".encode("utf-8")).decode("ascii")

    REAL_TOKEN = real_token
    PROFILE = profile
    TARGET_HOSTS = target_hosts
    AUTHORIZATION_HEADER = auth
    SANDBOX_CONFIG_DIR = sandbox_config_dir


def handle_request(request):
    request_id = request.get("id")
    if request_id is None:
        raise ValueError("request missing id")

    host = (request.get("host") or "").strip().lower()
    if host in TARGET_HOSTS and AUTHORIZATION_HEADER:
        return {"id": request_id, "headers": {"authorization": AUTHORIZATION_HEADER}}
    if host == "harnx.invalid" and request.get("path") == "/jira-auth-hook/debug":
        return {
            "id": request_id,
            "respond": {
                "status": 200,
                "headers": {"content-type": "application/json"},
                "body": json.dumps({"acli_config_dir": str(SANDBOX_CONFIG_DIR) if SANDBOX_CONFIG_DIR else None}),
            },
        }
    return {"id": request_id}


def main():
    try:
        initialize()
    except Exception as exc:
        log(f"startup failed: {exc}")
    sys.stdout.write("READY\n")
    sys.stdout.flush()

    for raw_line in sys.stdin:
        raw_line = raw_line.strip()
        if not raw_line:
            continue
        request_id = None
        try:
            request = json.loads(raw_line)
            if isinstance(request, dict):
                request_id = request.get("id")
            response = handle_request(request)
        except Exception as exc:
            log(f"request handling failed: {exc}; line={raw_line!r}")
            if request_id is None:
                continue
            response = {"id": request_id}
        emit(response)


if __name__ == "__main__":
    main()
