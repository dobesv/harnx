#!/usr/bin/env python3
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime
from pathlib import Path

try:
    import jwt as pyjwt
except ImportError:
    pyjwt = None

API_BEARER_HOSTS = {
    "api.github.com",
    "uploads.github.com",
    "objects.githubusercontent.com",
}
GIT_BASIC_HOSTS = {"github.com"}
GITHUB_ACCEPT = "application/vnd.github+json"
GITHUB_API_VERSION = "2022-11-28"
JWT_REFRESH_SKEW_SECONDS = 30
TOKEN_REFRESH_SKEW_SECONDS = 5 * 60
JWT_LIFETIME_SECONDS = 10 * 60
TEST_TOKEN_ENV = "GITHUB_APP_INSTALLATION_TOKEN"
TEST_TOKEN_EXPIRY_ENV = "GITHUB_APP_INSTALLATION_TOKEN_EXPIRES_AT"
TEST_COUNTER_FILE_ENV = "GITHUB_APP_TEST_COUNTER_FILE"
APP_ID_ENV = "GITHUB_APP_ID"
PRIVATE_KEY_ENV = "GITHUB_APP_PRIVATE_KEY"
INSTALLATION_ID_ENV = "GITHUB_APP_INSTALLATION_ID"
OWNER_ENV = "GITHUB_OWNER"
REPO_ENV = "GITHUB_REPO"
ORG_ENV = "GITHUB_ORG"
REPOSITORIES_ENV = "GITHUB_APP_REPOSITORIES"
PERMISSIONS_ENV = "GITHUB_APP_PERMISSIONS"
API_BASE_ENV = "GITHUB_APP_API_BASE"
DEFAULT_API_BASE = "https://api.github.com"

_cached_token = None
_cached_expiry = 0.0
_cached_jwt = None
_cached_jwt_expiry = 0.0
_cached_installation_id = None


def log(message):
    print(message, file=sys.stderr, flush=True)


def emit(response):
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()


def get_api_base():
    value = os.environ.get(API_BASE_ENV, DEFAULT_API_BASE).rstrip("/")
    parsed = urllib.parse.urlparse(value)
    if not parsed.scheme or not parsed.netloc:
        raise RuntimeError(
            f"{API_BASE_ENV} is not a valid URL: {value!r} (missing scheme or host)"
        )
    if parsed.scheme not in ("http", "https"):
        raise RuntimeError(
            f"{API_BASE_ENV} has unsupported scheme: {parsed.scheme!r} (expected http or https)"
        )
    return value


def normalize_host(host):
    return (host or "").strip().lower()


def parse_expiry(value, default_now_plus_seconds):
    if not value:
        return time.time() + default_now_plus_seconds
    try:
        if value.isdigit():
            return float(value)
        if value.endswith("Z"):
            value = value[:-1] + "+00:00"
        return datetime.fromisoformat(value).timestamp()
    except Exception:
        log(f"failed to parse expiry timestamp {value!r}; using fallback")
        return time.time() + default_now_plus_seconds


def load_required_env(name):
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"required env missing: {name}")
    return value


def increment_test_counter_if_needed():
    counter_path = os.environ.get(TEST_COUNTER_FILE_ENV)
    if not counter_path:
        return
    path = Path(counter_path)
    count = 0
    if path.exists():
        try:
            count = int(path.read_text(encoding="utf-8").strip() or "0")
        except Exception:
            count = 0
    path.write_text(str(count + 1), encoding="utf-8")


def base64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def sign_with_openssl(signing_input, key_path):
    # Shell-only host fallback for RS256 if PyJWT missing:
    # openssl dgst -sha256 -sign "$GITHUB_APP_PRIVATE_KEY"
    completed = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", str(key_path)],
        input=signing_input,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    )
    return completed.stdout


def mint_app_jwt():
    global _cached_jwt, _cached_jwt_expiry

    now = time.time()
    if _cached_jwt and now < (_cached_jwt_expiry - JWT_REFRESH_SKEW_SECONDS):
        return _cached_jwt

    app_id = load_required_env(APP_ID_ENV)
    key_path = Path(load_required_env(PRIVATE_KEY_ENV)).expanduser()
    pem = key_path.read_text(encoding="utf-8")
    issued_at = int(now) - 60
    expires_at = int(now) + JWT_LIFETIME_SECONDS
    payload = {"iat": issued_at, "exp": expires_at, "iss": app_id}

    if pyjwt is not None:
        token = pyjwt.encode(payload, pem, algorithm="RS256")
        if isinstance(token, bytes):
            token = token.decode("utf-8")
    else:
        header = {"alg": "RS256", "typ": "JWT"}
        signing_input = (
            f"{base64url(json.dumps(header, separators=(',', ':')).encode('utf-8'))}."
            f"{base64url(json.dumps(payload, separators=(',', ':')).encode('utf-8'))}"
        )
        signature = sign_with_openssl(signing_input.encode("utf-8"), key_path)
        token = f"{signing_input}.{base64url(signature)}"

    _cached_jwt = token
    _cached_jwt_expiry = float(expires_at)
    return token


def github_request(method, path, token, body=None):
    url = get_api_base() + path
    headers = {
        "Authorization": f"Bearer {token}",
        "Accept": GITHUB_ACCEPT,
        "X-GitHub-Api-Version": GITHUB_API_VERSION,
        "User-Agent": "harnx-github-app-auth-hook/1",
    }
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            payload = response.read().decode("utf-8")
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"GitHub API {method} {path} failed: {exc.code} {detail}") from exc
    except Exception as exc:
        raise RuntimeError(f"GitHub API {method} {path} failed: {exc}") from exc

    if not payload:
        return {}
    return json.loads(payload)


def parse_json_env(name, expected_type):
    raw = os.environ.get(name, "").strip()
    if not raw:
        return None
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"{name} must be valid JSON: {exc}") from exc
    if not isinstance(parsed, expected_type):
        raise RuntimeError(f"{name} must decode to {expected_type.__name__}")
    return parsed


def build_exchange_body():
    body = {}
    repositories = parse_json_env(REPOSITORIES_ENV, list)
    permissions = parse_json_env(PERMISSIONS_ENV, dict)
    if repositories:
        body["repositories"] = repositories
    if permissions:
        body["permissions"] = permissions
    return body or None


def resolve_installation_id(jwt_token):
    global _cached_installation_id

    if _cached_installation_id:
        return _cached_installation_id

    configured = os.environ.get(INSTALLATION_ID_ENV, "").strip()
    if configured:
        _cached_installation_id = configured
        return _cached_installation_id

    owner = os.environ.get(OWNER_ENV, "").strip()
    repo = os.environ.get(REPO_ENV, "").strip()
    org = os.environ.get(ORG_ENV, "").strip()

    if owner and repo:
        payload = github_request("GET", f"/repos/{owner}/{repo}/installation", jwt_token)
    elif org or owner:
        target_org = org or owner
        payload = github_request("GET", f"/orgs/{target_org}/installation", jwt_token)
    else:
        raise RuntimeError(
            "set GITHUB_APP_INSTALLATION_ID or GITHUB_OWNER/GITHUB_REPO or GITHUB_ORG"
        )

    installation_id = payload.get("id")
    if not installation_id:
        raise RuntimeError("installation lookup response missing id")
    _cached_installation_id = str(installation_id)
    return _cached_installation_id


def exchange_installation_token():
    jwt_token = mint_app_jwt()
    installation_id = resolve_installation_id(jwt_token)
    body = build_exchange_body()
    payload = github_request(
        "POST",
        f"/app/installations/{installation_id}/access_tokens",
        jwt_token,
        body=body,
    )
    token = payload.get("token")
    expires_at = payload.get("expires_at")
    if not token or not expires_at:
        raise RuntimeError("installation token response missing token or expires_at")
    return token, parse_expiry(expires_at, 55 * 60)


def get_installation_token():
    global _cached_token, _cached_expiry

    now = time.time()
    if _cached_token and now < (_cached_expiry - TOKEN_REFRESH_SKEW_SECONDS):
        return _cached_token

    override_token = os.environ.get(TEST_TOKEN_ENV, "").strip()
    if override_token:
        increment_test_counter_if_needed()
        _cached_token = override_token
        _cached_expiry = parse_expiry(os.environ.get(TEST_TOKEN_EXPIRY_ENV, ""), 55 * 60)
        return _cached_token

    token, expiry = exchange_installation_token()
    _cached_token = token
    _cached_expiry = expiry
    return _cached_token


def github_basic_auth(token):
    blob = base64.b64encode(f"x-access-token:{token}".encode("utf-8")).decode("ascii")
    return f"Basic {blob}"


def handle_request(request):
    request_id = request.get("id")
    if request_id is None:
        raise ValueError("request missing id")

    host = normalize_host(request.get("host"))
    if host in API_BEARER_HOSTS:
        token = get_installation_token()
        return {"id": request_id, "headers": {"authorization": f"Bearer {token}"}}
    if host in GIT_BASIC_HOSTS:
        token = get_installation_token()
        return {"id": request_id, "headers": {"authorization": github_basic_auth(token)}}
    return {"id": request_id}


def main():
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
            # Fail closed: do not log raw request line (may contain sensitive data)
            # Log only non-sensitive diagnostics
            log(f"request handling failed: {type(exc).__name__}")
            if request_id is None:
                continue
            response = {"id": request_id}
        emit(response)


if __name__ == "__main__":
    main()
