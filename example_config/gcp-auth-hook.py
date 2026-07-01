#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time

TOKEN_CMD_ENV = "HARNX_GCP_TOKEN_CMD"
DEFAULT_TOKEN_CMD = "gcloud auth print-access-token"
TOKEN_CMD_TIMEOUT_ENV = "HARNX_GCP_TOKEN_CMD_TIMEOUT_SECONDS"
DEFAULT_TOKEN_CMD_TIMEOUT_SECONDS = 10
DEFAULT_TOKEN_TTL_SECONDS = 55 * 60
REFRESH_SKEW_SECONDS = 60

# Keep Google API host matching in one predicate so operators can extend it if
# e2e shows a 401 on an unexpected Google host that still needs auth injection.
GOOGLE_API_HOST_SUFFIXES = [
    "googleapis.com",
]

_cached_token = None
_cached_expiry = 0.0


def log(message):
    print(message, file=sys.stderr, flush=True)


def is_google_api_host(host):
    host = (host or "").strip().lower()
    return any(host == suffix or host.endswith("." + suffix) for suffix in GOOGLE_API_HOST_SUFFIXES)


def reset_cached_token():
    global _cached_token, _cached_expiry
    _cached_token = None
    _cached_expiry = 0.0


def token_cmd_timeout_seconds():
    raw_value = os.environ.get(TOKEN_CMD_TIMEOUT_ENV)
    if raw_value is None:
        return DEFAULT_TOKEN_CMD_TIMEOUT_SECONDS
    try:
        timeout = float(raw_value)
    except ValueError:
        log(
            f"invalid {TOKEN_CMD_TIMEOUT_ENV} value {raw_value!r}; "
            f"using default timeout={DEFAULT_TOKEN_CMD_TIMEOUT_SECONDS}s"
        )
        return DEFAULT_TOKEN_CMD_TIMEOUT_SECONDS
    if timeout <= 0:
        log(
            f"non-positive {TOKEN_CMD_TIMEOUT_ENV} value {raw_value!r}; "
            f"using default timeout={DEFAULT_TOKEN_CMD_TIMEOUT_SECONDS}s"
        )
        return DEFAULT_TOKEN_CMD_TIMEOUT_SECONDS
    return timeout


def mint_token():
    global _cached_token, _cached_expiry

    now = time.time()
    if _cached_token and now < (_cached_expiry - REFRESH_SKEW_SECONDS):
        return _cached_token

    token_cmd = os.environ.get(TOKEN_CMD_ENV, DEFAULT_TOKEN_CMD)
    timeout_seconds = token_cmd_timeout_seconds()
    try:
        completed = subprocess.run(
            token_cmd,
            shell=True,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as exc:
        log(f"token command timed out after {exc.timeout}s: {exc.__class__.__name__}")
        reset_cached_token()
        return None
    except Exception as exc:
        log(f"token command failed: {exc.__class__.__name__}: {exc}")
        reset_cached_token()
        return None

    token = completed.stdout.strip()
    if not token:
        log("token command produced empty stdout")
        reset_cached_token()
        return None

    _cached_token = token
    _cached_expiry = now + DEFAULT_TOKEN_TTL_SECONDS
    return _cached_token


def metadata_headers(content_type):
    headers = {"metadata-flavor": "Google"}
    if content_type:
        headers["content-type"] = content_type
    return headers


def metadata_response(path):
    token_prefix = "/computeMetadata/v1/instance/service-accounts/"
    if path.startswith(token_prefix) and path.endswith("/token"):
        body = json.dumps(
            {
                "access_token": "proxy-managed",
                "expires_in": 3600,
                "token_type": "Bearer",
            }
        )
        return {
            "status": 200,
            "headers": metadata_headers("application/json"),
            "body": body,
        }

    known_ok_paths = {
        "/computeMetadata/",
        "/computeMetadata/v1/",
        "/computeMetadata/v1/instance",
        "/computeMetadata/v1/instance/",
        "/computeMetadata/v1/project/project-id",
    }
    if path in known_ok_paths:
        body = "{}"
        if path == "/computeMetadata/v1/project/project-id":
            body = "proxy-project"
        return {
            "status": 200,
            "headers": metadata_headers("application/json" if body.startswith("{") else "text/plain"),
            "body": body,
        }

    return {
        "status": 404,
        "headers": metadata_headers("application/json"),
        "body": "{}",
    }


def handle_request(request):
    request_id = request.get("id")
    if request_id is None:
        raise ValueError("request missing id")

    path = request.get("path") or ""
    host = request.get("host") or ""

    if path.startswith("/computeMetadata/"):
        return {"id": request_id, "respond": metadata_response(path)}

    if is_google_api_host(host):
        token = mint_token()
        if token:
            return {"id": request_id, "headers": {"authorization": f"Bearer {token}"}}
        return {"id": request_id}

    return {"id": request_id}


def emit(response):
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()


def main():
    sys.stdout.write("READY\n")
    sys.stdout.flush()

    for raw_line in sys.stdin:
        raw_line = raw_line.strip()
        if not raw_line:
            continue
        request_id = None
        request_host = ""
        request_path = ""
        try:
            request = json.loads(raw_line)
            if isinstance(request, dict):
                request_id = request.get("id")
                request_host = request.get("host") or ""
                request_path = request.get("path") or ""
            response = handle_request(request)
        except Exception as exc:
            log(
                f"request handling failed: {exc.__class__.__name__}: {exc}; "
                f"id={request_id!r} host={request_host!r} path={request_path!r}"
            )
            if request_id is None:
                continue
            response = {"id": request_id}
        emit(response)


if __name__ == "__main__":
    main()
