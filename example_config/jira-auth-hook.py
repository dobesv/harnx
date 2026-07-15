#!/usr/bin/env python3
import base64
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
import traceback
from pathlib import Path

SENTINEL_TOKEN_BLOB = (
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6OjowMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBh"
    "NmM2MzI1MzM1NGQxODBiNjkzYWFjYmRkZjlmYjA2YzFkMGI2NmE0MmQ4Mzc1NmJjM2U5ZjM5ODg4MzRh"
    "MGZiM2EzYTRhMWY="
)
TOKEN_CMD_ENV = "HARNX_JIRA_TOKEN_CMD"
# Per-platform token lookup. macOS stores the acli token in the login keychain
# (service "acli", account "jira:<profile>"); Linux uses libsecret via
# secret-tool. Override either with HARNX_JIRA_TOKEN_CMD.
LINUX_TOKEN_CMD = "secret-tool lookup service acli username {profile_arg}"
MACOS_TOKEN_CMD = "security find-generic-password -s acli -a {profile_arg} -w"


def default_token_cmd():
    return MACOS_TOKEN_CMD if sys.platform == "darwin" else LINUX_TOKEN_CMD
HOST_CONFIG_ENV_NAMES = ["ACLI_HOST_CONFIG", "HARNX_JIRA_HOST_CONFIG"]
DEFAULT_HOST_CONFIG = "~/.config/acli/jira_config.yaml"
TEMP_ROOT_ENV_NAMES = ["HARNX_JIRA_TEMP_ROOT", "TEMP_FILE_ROOT", "TMPDIR", "TMP", "TEMP"]
DEFAULT_TEMP_ROOT = "/tmp"
# Optional: set to a path to append diagnostic logs (never contains the token).
LOG_FILE_ENV = "HARNX_JIRA_LOG_FILE"

REAL_TOKEN = None
PROFILE = None
# The `temp_file_root` the proxy sends in startup/per-request `vars`, captured
# before init runs. See resolve_temp_root() for how it's used.
TEMP_ROOT_FROM_REQUEST = None
TARGET_HOSTS = set()
AUTHORIZATION_HEADER = None
SANDBOX_CONFIG_DIR = None
# Set to the initialize() error string when startup fails; surfaced via the
# debug endpoint so the failure reason is inspectable without reading the log.
INIT_ERROR = None
# The last error already surfaced to the UI, so lazy-init retries don't repeat
# the same notice on every request.
LAST_NOTICED_ERROR = None


def log(message):
    """Log to stderr and, if HARNX_JIRA_LOG_FILE is set, append to that file.

    Never log the token itself — only lengths/booleans.
    """
    line = f"{time.strftime('%Y-%m-%d %H:%M:%S')} [jira-auth-hook] {message}"
    print(line, file=sys.stderr, flush=True)
    path = os.environ.get(LOG_FILE_ENV)
    if path:
        try:
            with open(Path(path).expanduser(), "a", encoding="utf-8") as handle:
                handle.write(line + "\n")
        except Exception:
            pass


def trace(message):
    """Verbose per-request logging, enabled only when HARNX_JIRA_LOG_FILE is set.

    Every request the proxy sees passes through this hook, so unconditional
    logging would spam stderr during normal runs. When you're diagnosing acli
    auth you set HARNX_JIRA_LOG_FILE, which turns this on so the log shows the
    method + host + path + injection decision for every outbound request.
    """
    if os.environ.get(LOG_FILE_ENV):
        log(message)


def notice(message, level="error"):
    """Surface a message to the harnx UI via the structured stdout channel.

    Emits a standalone `{"notice": {...}}` line (no request id) that the auth
    proxy forwards up to harnx, which posts it as a user-visible Notice.
    """
    emit({"notice": {"level": level, "message": message}})
    log(f"NOTICE[{level}]: {message}")


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
    """Parse acli's jira_config.yaml (small, fixed-shape YAML) with stdlib only.

    Uses PyYAML when available; otherwise falls back to an indentation-agnostic
    line parser that tolerates any indent for the `profiles:` list items.
    """
    with open(path, "r", encoding="utf-8") as handle:
        text = handle.read()

    current_profile, profiles = _parse_host_config_text(text)

    if not current_profile:
        raise ValueError("current_profile missing from host config")

    log(f"parsed {len(profiles)} profile(s); current_profile={current_profile!r}")
    for profile in profiles:
        profile_key = f"{profile.get('cloud_id', '')}:{profile.get('account_id', '')}"
        if profile_key == current_profile:
            return current_profile, profile

    keys = [f"{p.get('cloud_id', '')}:{p.get('account_id', '')}" for p in profiles]
    raise ValueError(
        f"profile matching current_profile not found: {current_profile!r}; parsed keys={keys}"
    )


def _extract_profiles(data):
    """Pull (current_profile, [profile_dict, ...]) from a parsed config dict."""
    profiles = [p for p in (data.get("profiles") or []) if isinstance(p, dict)]
    cp = data.get("current_profile")
    return (str(cp) if cp is not None else None), profiles


def _parse_via_pyyaml(text):
    try:
        import yaml  # type: ignore
    except Exception:
        return None
    try:
        data = yaml.safe_load(text)
    except Exception:
        return None
    return data if isinstance(data, dict) else None


def _parse_via_yq(text):
    """Convert YAML->JSON with the `yq` CLI, if present, then parse the JSON.

    Handles both implementations: mikefarah/yq (Go) needs `-o=json`; kislyuk's
    python `yq` emits JSON by default. We try both and take the first that
    yields a JSON object.
    """
    if not shutil.which("yq"):
        return None
    for cmd in (["yq", "-o=json", "."], ["yq", "."]):
        try:
            proc = subprocess.run(
                cmd,
                input=text,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=10,
            )
        except Exception:
            continue
        if proc.returncode != 0 or not proc.stdout.strip():
            continue
        try:
            data = json.loads(proc.stdout)
        except Exception:
            continue
        if isinstance(data, dict):
            return data
    return None


def _indent_of(text):
    return len(text) - len(text.lstrip(" \t"))


def _is_profile_item(stripped):
    return stripped == "-" or stripped.startswith("- ")


def _assign_profile_key(profile, pair_text):
    """Store a `key: value` scalar on `profile`; ignore non-pairs."""
    if not pair_text or ":" not in pair_text:
        return
    key, value = pair_text.split(":", 1)
    profile[key.strip()] = parse_scalar(value)


def _consume_profiles_line(state, stripped):
    """Handle a line known to belong to the `profiles:` list."""
    if _is_profile_item(stripped):
        current = {}
        state["profiles"].append(current)
        state["current"] = current
        rest = stripped[2:].strip() if stripped.startswith("- ") else ""
        _assign_profile_key(current, rest)
    elif state["current"] is not None:
        _assign_profile_key(state["current"], stripped)


def _consume_toplevel_line(state, stripped, indent):
    """Handle a top-level (or dedented) line, updating parser state."""
    state["in_profiles"] = False
    state["current"] = None
    if stripped.startswith("current_profile:"):
        state["current_profile"] = parse_scalar(stripped.split(":", 1)[1])
    elif stripped.startswith("profiles:"):
        state["in_profiles"] = True
        state["profiles_indent"] = indent


def _parse_host_config_line(state, line):
    """Route one config line to the profiles-list or top-level handler.

    A list item (any indent) starts a new profile; a deeper-indented
    "key: value" belongs to the current one. YAML allows the "-" at the same
    indent as `profiles:`, so match on the item marker, not a fixed width.
    """
    stripped = line.strip()
    if not stripped or stripped.startswith("#"):
        return
    indent = _indent_of(line)
    in_list = state["in_profiles"] and (
        _is_profile_item(stripped) or indent > state["profiles_indent"]
    )
    if in_list:
        _consume_profiles_line(state, stripped)
    else:
        _consume_toplevel_line(state, stripped, indent)


def _parse_host_config_text(text):
    """Return (current_profile, [profile_dict, ...]) from config text.

    Tiered: PyYAML in-process, then the `yq` CLI, then a stdlib line parser.
    """
    data = _parse_via_pyyaml(text)
    method = "pyyaml"
    if data is None:
        data = _parse_via_yq(text)
        method = "yq"
    if isinstance(data, dict):
        log(f"parsed config via {method}")
        return _extract_profiles(data)

    log("parsed config via builtin line parser")
    state = {
        "current_profile": None,
        "profiles": [],
        "current": None,
        "in_profiles": False,
        "profiles_indent": 0,
    }
    for raw_line in text.splitlines():
        _parse_host_config_line(state, raw_line.rstrip("\n"))

    return state["current_profile"], state["profiles"]


def resolve_host_config_path():
    for env_name in HOST_CONFIG_ENV_NAMES:
        candidate = os.environ.get(env_name)
        if candidate:
            return Path(candidate).expanduser()
    return Path(DEFAULT_HOST_CONFIG).expanduser()


def resolve_temp_root():
    # Highest precedence: the `temp_file_root` the proxy sends in each request's
    # `vars` — proxy-auth's own per-instance temp dir (unique, auto-deleted on
    # exit). This is what ACLI_CONFIG_DIR (set via --env from the SAME
    # $temp_file_root) points the sandboxed acli at, so both sides agree without
    # guessing a shared path.
    if TEMP_ROOT_FROM_REQUEST:
        return Path(TEMP_ROOT_FROM_REQUEST).expanduser()
    for env_name in TEMP_ROOT_ENV_NAMES:
        candidate = os.environ.get(env_name)
        if candidate:
            return Path(candidate).expanduser()
    return Path(DEFAULT_TEMP_ROOT)


def lookup_token(current_profile):
    custom = os.environ.get(TOKEN_CMD_ENV)
    token_cmd = custom or default_token_cmd().format(
        profile_arg=shlex.quote(f"jira:{current_profile}")
    )
    if custom:
        source = "HARNX_JIRA_TOKEN_CMD"
    elif sys.platform == "darwin":
        source = "macOS keychain"
    else:
        source = "secret-tool"
    log(f"token lookup via {source}: {token_cmd!r}")
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
        stderr = getattr(exc, "stderr", "") or ""
        raise RuntimeError(
            f"token command failed: {token_cmd!r}: {exc}; stderr={stderr.strip()!r}"
        ) from exc

    token = completed.stdout.strip()
    if not token:
        raise RuntimeError(
            f"token command produced empty stdout: {token_cmd!r}; "
            f"stderr={completed.stderr.strip()!r}"
        )
    log(f"token lookup ok ({len(token)} chars)")
    return token


def write_synthetic_config(current_profile, profile, temp_root):
    config_root = temp_root / "harnx-fs-acli" / "acli"
    config_root.mkdir(parents=True, exist_ok=True)
    config_path = config_root / "jira_config.yaml"
    email = profile.get("email") or os.environ.get("ATLASSIAN_EMAIL", "")
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
            # acli stores the token as an encrypted SecretStore blob that it
            # expects as a YAML `!!binary` scalar: the YAML parser base64-decodes
            # it before acli decrypts. Writing it as a plain string makes acli
            # fail to decrypt and abort ("failed to retrieve authenticated
            # status") BEFORE it ever calls api.atlassian.com — so the proxy's
            # on-the-wire token swap never runs. Must stay `!!binary`.
            f"      token: !!binary {SENTINEL_TOKEN_BLOB}",
            "",
        ]
    )
    config_path.write_text(config_text, encoding="utf-8")
    return email, config_root.parent


def initialize():
    global REAL_TOKEN, PROFILE, TARGET_HOSTS, AUTHORIZATION_HEADER, SANDBOX_CONFIG_DIR

    host_config = resolve_host_config_path()
    log(f"host config path: {host_config}")
    current_profile, profile = load_host_config(host_config)
    real_token = lookup_token(current_profile)
    temp_root = resolve_temp_root()
    email, sandbox_config_dir = write_synthetic_config(current_profile, profile, temp_root)
    if not email:
        log("WARNING: no email for profile (set ATLASSIAN_EMAIL) — Basic auth user will be blank")
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
    log(
        f"initialized: email={email!r} site={site!r} "
        f"synthetic_config_dir={sandbox_config_dir} "
        f"(injects auth only for {sorted(target_hosts)}; NOT as.atlassian.com)"
    )


def ensure_initialized():
    """Initialize lazily on first use and retry on failure.

    Reading the acli config + keyring happens on the first Atlassian request,
    not at startup — so we never touch the keyring before it's ready (or at all
    if no Atlassian request ever arrives), and a transient failure is retried on
    the next request instead of being cached for the process's lifetime.
    Returns True once auth is ready.
    """
    global INIT_ERROR, LAST_NOTICED_ERROR
    if AUTHORIZATION_HEADER is not None:
        return True
    try:
        initialize()
        INIT_ERROR = None
        LAST_NOTICED_ERROR = None
        return True
    except Exception as exc:
        INIT_ERROR = str(exc)
        log(f"initialize failed (will retry on next request): {exc}")
        log(traceback.format_exc().rstrip())
        # Surface to the UI once per distinct error (retries won't re-spam).
        if INIT_ERROR != LAST_NOTICED_ERROR:
            notice(f"Atlassian auth unavailable — acli/Jira calls will fail: {INIT_ERROR}")
            LAST_NOTICED_ERROR = INIT_ERROR
        return False


def capture_temp_root(vars_block):
    # Capture the proxy-provided temp root (see resolve_temp_root) before any
    # init runs. Only the first non-empty value matters — it's stable for the
    # proxy's lifetime — and we never overwrite it with a later empty one.
    global TEMP_ROOT_FROM_REQUEST
    if not TEMP_ROOT_FROM_REQUEST and vars_block.get("temp_file_root"):
        TEMP_ROOT_FROM_REQUEST = vars_block["temp_file_root"]


def handle_startup(request):
    request_id = request.get("id")
    if request_id is None:
        raise ValueError("request missing id")

    vars_block = request.get("vars") or {}
    capture_temp_root(vars_block)
    try:
        ensure_initialized()
    except Exception:
        pass
    env = {}
    if SANDBOX_CONFIG_DIR:
        env["ACLI_CONFIG_DIR"] = str(SANDBOX_CONFIG_DIR)
    return {"id": request_id, "env": env}


def _maybe_inject_auth(request_id, host, endpoint, is_atlassian):
    """Return an auth-injection response for target hosts, else None.

    Inject the api_token ONLY for the hosts acli authenticates to with it: the
    `api.atlassian.com` CLI gateway (GET /cli/<cloud_id>/...) and the site's
    REST API (TARGET_HOSTS). Do NOT inject for `as.atlassian.com` — acli calls
    its `/api/v1/batch` endpoint UNAUTHENTICATED (Basic BLANK), and forcing a
    real token there makes the request fail ("unauthorized"), aborting acli
    before it reaches the working api.atlassian.com data call.
    """
    if host in TARGET_HOSTS:
        if AUTHORIZATION_HEADER:
            log(f"injecting auth for {endpoint}")
            return {"id": request_id, "headers": {"authorization": AUTHORIZATION_HEADER}}
        log(
            f"{endpoint} matched a target host but auth is unavailable — NOT injecting"
            + (f" (init error: {INIT_ERROR})" if INIT_ERROR else "")
        )
    elif is_atlassian:
        # Atlassian host we deliberately don't authenticate (e.g. as.atlassian.com).
        trace(
            f"pass-through (atlassian host not in target set {sorted(TARGET_HOSTS)}): {endpoint}"
        )
    return None


def _debug_response(request_id):
    ensure_initialized()
    return {
        "id": request_id,
        "respond": {
            "status": 200,
            "headers": {"content-type": "application/json"},
            "body": json.dumps(
                {
                    "initialized": AUTHORIZATION_HEADER is not None,
                    "acli_config_dir": str(SANDBOX_CONFIG_DIR) if SANDBOX_CONFIG_DIR else None,
                    "target_hosts": sorted(TARGET_HOSTS),
                    "error": INIT_ERROR,
                }
            ),
        },
    }


def handle_request(request):
    request_id = request.get("id")
    if request_id is None:
        raise ValueError("request missing id")

    capture_temp_root(request.get("vars") or {})

    host = (request.get("host") or "").strip().lower()
    method = (request.get("method") or "?").upper()
    path = request.get("path") or ""
    endpoint = f"{method} https://{host}{path}"

    is_atlassian = host.endswith(".atlassian.com") or host.endswith(".atlassian.net")
    # Trace every request when logging is enabled so the log shows exactly which
    # hosts/paths acli contacts (the key clue when "zero injections" happen).
    trace(f"request {endpoint} (atlassian={is_atlassian}, target={host in TARGET_HOSTS})")

    # Initialize lazily on the first Atlassian-looking request (any *.atlassian
    # host), so the keyring is touched only when needed.
    if is_atlassian:
        ensure_initialized()

    injected = _maybe_inject_auth(request_id, host, endpoint, is_atlassian)
    if injected is not None:
        return injected

    if host == "harnx.invalid" and path == "/jira-auth-hook/debug":
        return _debug_response(request_id)

    return {"id": request_id}


def main():
    # Startup can now eagerly initialize from an `event: "startup"` message so
    # the synthetic acli config exists before the sandboxed command runs. The
    # per-request ensure_initialized() calls remain as backward-compatible
    # fallback when no startup message arrives.
    log(f"starting (pid {os.getpid()}) — startup init when requested, lazy fallback otherwise")
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
            if request.get("event") == "startup":
                response = handle_startup(request)
            else:
                response = handle_request(request)
        except Exception as exc:
            log(f"request handling failed: {exc}; line={raw_line!r}")
            if request_id is None:
                continue
            response = {"id": request_id}
        emit(response)


if __name__ == "__main__":
    main()
