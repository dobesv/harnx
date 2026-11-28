#!/usr/bin/env bash
# Run from the delivery worktree (the tool script cd's into it) through
# Pantheon's registered publish_plan tool, with the ambient GitHub credential
# route (GH_TOKEN/GITHUB_TOKEN or gh config) that wait_for_pr_stable uses.
set -eu
command -v python3 >/dev/null || { echo 'publish-plan: python3 is required' >&2; exit 1; }
exec python3 -I - "$@" <<'PYTHON'
import argparse
import base64
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile


MAX_BYTES = 5 * 1024 * 1024
MAX_ATTEMPTS = 3
MESSAGE = b"Publish plan snapshot\n"
IDENTITY = {
    f"GIT_{role}_{field}": value
    for role in ("AUTHOR", "COMMITTER")
    for field, value in (("NAME", "Pantheon"), ("EMAIL", "pantheon@plan-snapshot.invalid"))
}
# Nothing from the caller's HTTP environment or Git context rides along by default.
BASE_ENV = {"PATH": os.environ.get("PATH", os.defpath), "LC_ALL": "C"}
GIT_ENV = dict(BASE_ENV, GIT_TERMINAL_PROMPT="0", GIT_NO_REPLACE_OBJECTS="1", GIT_NO_LAZY_FETCH="1")
GIT_CONFIG_ENV = re.compile(r"GIT_CONFIG_(?:GLOBAL|SYSTEM|NOSYSTEM|COUNT|PARAMETERS|(?:KEY|VALUE)_[0-9]+)")
GITHUB_URL = re.compile(r"https://github\.com/([a-z0-9-]+)/([a-z0-9_.-]+?)(?:\.git)?/?", re.I | re.ASCII)
FILENAME = re.compile(rb"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\.md")
FINDING_LIMIT = 50
PATTERNS = {
    "token": rb"\b(?:gh[pousr]_[a-z0-9]{20,}|github_pat_[a-z0-9_]{20,}|(?:AKIA|ASIA)[A-Z0-9]{16}|"
             rb"xox[baprs]-[a-z0-9-]{10,}|sk-(?:proj-|ant-)?[a-z0-9_-]{16,}|AIza[a-z0-9_-]{35})\b"
             rb"|\b(?:bearer[ \t]+|(?:api[_ -]?key|(?:access[_ -]?)?token|password|secret)[ \t]*[:=][ \t]*[\"']?)"
             rb"[a-z0-9][a-z0-9._~+/=-]{7,}",
    "private_key": rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
    "jwt": rb"\beyJ[a-z0-9_-]+\.eyJ[a-z0-9_-]+\.[a-z0-9_-]+\b",
    "url_credentials": rb"\b[a-z][a-z0-9+.-]*://[^\s/?#<>\"'@]+@",
    "email": rb"(?<![a-z0-9.!#$%&'*+/=?^_`{|}~-])[a-z0-9.!#$%&'*+/=?^_`{|}~-]{1,64}"
             rb"@([a-z0-9-]{1,63}(?:\.[a-z0-9-]{1,63}){0,10}\.[a-z]{2,63})(?![a-z0-9-])",
}
PATTERNS = {category: re.compile(pattern, re.I) for category, pattern in PATTERNS.items()}


class Refusal(Exception):
    """Only static diagnostics or explicitly validated metadata may reach tool logs."""


def run(command, env, *, cwd=None, data=None, timeout=120):
    try:
        return subprocess.run(
            command, input=data if data is not None else b"", stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, env=env, cwd=cwd, timeout=timeout, check=False,
        )
    except FileNotFoundError:
        raise Refusal(f"{command[0]} is required or its working directory is unavailable") from None
    except subprocess.TimeoutExpired:
        raise Refusal(f"{command[0]} timed out") from None


class Git:
    def __init__(self, cwd, env):
        self.cwd = cwd
        self.env = env

    def run(self, *args, data=None):
        return run(
            ["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false",
             "-c", "push.gpgSign=false", "-c", "gc.auto=0", *args],
            self.env, cwd=self.cwd, data=data,
        )

    def require(self, error, *args, data=None):
        result = self.run(*args, data=data)
        if result.returncode:
            raise Refusal(error)
        return result.stdout


def local_git():
    # Retain the effective configuration for remote.get-url (including rewrites),
    # but discover and update the actual working repository, not an ambient GIT_DIR.
    env = dict(GIT_ENV)
    env.update({k: v for k, v in os.environ.items()
                if k in ("HOME", "XDG_CONFIG_HOME") or GIT_CONFIG_ENV.fullmatch(k)})
    return Git(Path.cwd(), env)


def destination(git):
    identities = []
    for flag in ((), ("--push",)):
        raw = git.require("origin fetch and push URLs are required", "remote", "get-url", *flag, "--all", "origin")
        # Match one line exactly. Multiple effective URLs (even duplicates) are ambiguous.
        match = GITHUB_URL.fullmatch(os.fsdecode(raw.removesuffix(b"\n")))
        if not match or match[2] in (".", "..") or match[2].lower().endswith(".git"):
            raise Refusal("origin must have one credential-free GitHub HTTPS fetch and push URL")
        identities.append(f"{match[1]}/{match[2]}".lower())
    if identities[0] != identities[1]:
        raise Refusal("effective origin fetch and push destinations differ")
    return identities[0]


def source_path(source, root):
    path = Path(source)
    if not source or ".." in path.parts or source.endswith("/"):
        raise Refusal("source path must name a file without parent traversal")
    path = Path.cwd() / path
    # macOS exposes these system roots via symlinks. Trust only these fixed aliases,
    # never realpath() a supplied source or an untrusted TMPDIR.
    for temp in (Path("/tmp"), Path("/var/tmp")):
        if temp in path.parents:
            path = temp.resolve() / path.relative_to(temp)
            break
    if not any(base in path.parents for base in (root, Path("/tmp").resolve(), Path("/var/tmp").resolve())):
        raise Refusal("source must be inside the worktree or /tmp or /var/tmp")
    return path


def read_source(path):
    # Walk open directory descriptors and reject symlinks at every step. Opening
    # nonblocking avoids hanging on a FIFO swapped in before the fstat.
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        with ExitStack() as stack:
            fd = os.open(path.anchor, directory_flags)
            stack.callback(os.close, fd)
            for part in path.parts[1:-1]:
                fd = os.open(part, directory_flags, dir_fd=fd)
                stack.callback(os.close, fd)
            file_flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
            source_fd = os.open(path.name, file_flags, dir_fd=fd)
            stack.callback(os.close, source_fd)
            info = os.fstat(source_fd)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.geteuid():
                raise Refusal("source must be a regular, singly linked file owned by this user")
            with os.fdopen(source_fd, "rb", closefd=False) as source:
                data = source.read(MAX_BYTES + 1)
    except OSError:
        raise Refusal("could not open source safely (check file permissions and symlinks)") from None
    if not data or len(data) > MAX_BYTES:
        raise Refusal("source must contain between 1 byte and 5 MiB of Markdown")
    try:
        data.decode("utf-8")
        if b"\0" not in data:
            return data
    except UnicodeDecodeError:
        pass
    raise Refusal("source must be UTF-8 Markdown without NUL bytes")


def example_email(match):
    domain = match[1].lower()
    return domain.endswith((b".example", b".invalid", b".test")) or domain in (
        b"example.com", b"example.net", b"example.org",
    )


def scan(data):
    findings = []
    for number, line in enumerate(re.split(rb"\r\n|\r|\n", data), 1):
        for category, pattern in PATTERNS.items():
            for match in pattern.finditer(line):
                if category == "email" and example_email(match):
                    continue
                if len(findings) == FINDING_LIMIT:
                    return findings, True
                findings.append(f"{category} at line {number}, byte column {match.start() + 1}")
    return findings, False


def require_clean(data):
    findings, truncated = scan(data)
    if findings:
        if truncated:
            findings.append("additional findings omitted")
        raise Refusal("snapshot contains potentially sensitive content; sanitize and retry: " + "; ".join(findings))


def require_token(token, source):
    # The token reaches a line-oriented HTTP header (curl --header @- stdin)
    # and a Git config value. Control bytes would let a malformed credential
    # inject headers, so refuse it rather than strip or rewrite the value.
    if re.search(r"[\x00-\x1f\x7f]", token):
        raise Refusal(f"the GitHub token from {source} contains control bytes")
    return token


def gh_token():
    # The same credential route wait_for_pr_stable uses: GH_TOKEN/GITHUB_TOKEN
    # ambient passthrough, falling back to the gh CLI's config. The lookup is
    # pinned to github.com — unqualified, gh chooses a sole configured
    # Enterprise host before defaulting to github.com, which would send that
    # host's credential to the public endpoints used here. The token never
    # reaches a diagnostic; only its effect on the request does.
    for name in ("GH_TOKEN", "GITHUB_TOKEN"):
        value = os.environ.get(name, "")
        if value:
            return require_token(value, name)
    result = run(["gh", "auth", "token", "--hostname", "github.com"],
                 dict(os.environ, GH_PAGER=""), timeout=30)
    if result.returncode == 0:
        # gh prints the token with a trailing newline; surrounding whitespace
        # is framing, not part of the credential.
        token = require_token(result.stdout.decode("utf-8", "replace").strip(), "gh auth token")
        if token:
            return token
    raise Refusal("a GitHub token is required: set GH_TOKEN or GITHUB_TOKEN, or run gh auth login")


# Ambient egress configuration rides along on authenticated calls only; the
# anonymous probe stays stripped, so no proxy or credential can reach it.
EGRESS_ENV_KEYS = (
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    "http_proxy", "https_proxy", "all_proxy", "no_proxy",
    "SSL_CERT_FILE", "CURL_CA_BUNDLE", "GIT_SSL_CAINFO",
)


def egress_env():
    return {key: value for key, value in os.environ.items() if key in EGRESS_ENV_KEYS}


def api_request(repo, token=None):
    # -q must be first to disable curlrc. The anonymous probe runs with a fresh
    # environment and no proxy, credentials or custom CAs. No netrc, redirects
    # or cookies. The authenticated call reads its Authorization header from
    # stdin (--header @-), so the credential never appears in curl's argv,
    # where host observers can read it; it carries ambient egress configuration
    # alongside.
    headers = ["--header", "Accept: application/vnd.github+json"]
    data = None
    if token is None:
        route = ["--proxy", "", "--noproxy", "*"]
        env = BASE_ENV
    else:
        route = []
        env = {**BASE_ENV, **egress_env()}
        headers += ["--header", "@-"]
        data = f"Authorization: Bearer {token}\r\n".encode("ascii")
    result = run([
        "curl", "-q", "--silent", "--proto", "=https", "--connect-timeout", "10", "--max-time", "30",
        "--max-filesize", "1048576", *headers, "--write-out", "\n%{http_code}", *route,
        "--url", f"https://api.github.com/repos/{repo}",
    ], env, data=data, timeout=35)
    if result.returncode:
        label = "anonymous GitHub probe" if token is None else "authenticated GitHub confirmation"
        raise Refusal(f"{label} failed (curl exit {result.returncode}); visibility is unknown")
    body, _, status = result.stdout.rpartition(b"\n")
    return status, body


def require_visibility(repo, token):
    status, body = api_request(repo)
    expected = "public"
    if status == b"404":
        status, body = api_request(repo, token)
        expected = "private"
    if status != b"200":
        code = status.decode("ascii") if re.fullmatch(rb"[0-9]{3}", status) else "unknown"
        raise Refusal(f"GitHub visibility is unknown (HTTP {code})")
    try:
        metadata = json.loads(body)
    except (ValueError, RecursionError):
        raise Refusal("GitHub returned invalid visibility metadata") from None
    if not isinstance(metadata, dict) or not (
        str(metadata.get("full_name")).lower() == repo
        and str(metadata.get("html_url")).lower() == f"https://github.com/{repo}"
        and metadata.get("private") is (expected == "private")
        and metadata.get("visibility") == expected
    ):
        raise Refusal("GitHub returned inconsistent repository identity or visibility")


def report(payload):
    print(json.dumps(payload, separators=(",", ":")))


class History:
    def __init__(self, git, remote, ref, oid_length):
        self.git = git
        self.remote = remote
        self.ref = ref
        self.oid = re.compile(rb"[a-f0-9]{%d}" % oid_length)
        self.zero = "0" * oid_length
        self.ref_bytes = os.fsencode(ref)

    def oid_from(self, raw):
        raw = raw.removesuffix(b"\n")
        if not self.oid.fullmatch(raw):
            raise Refusal("Git returned an invalid object ID")
        return raw.decode("ascii")

    def remote_tip(self):
        reply = self.git.run("ls-remote", "--exit-code", "--refs", self.remote, self.ref)
        if reply.returncode == 2:
            return None
        if reply.returncode:
            raise Refusal("could not look up remote plan ref; check the GitHub token and repository access")
        matches = [line.split(b"\t")[0] for line in reply.stdout.split(b"\n")
                   if line.endswith(b"\t" + self.ref_bytes)]
        if not matches:  # ls-remote patterns also match at a slash boundary.
            return None
        if len(matches) != 1:
            raise Refusal("remote plan ref is ambiguous")
        self.oid_from(matches[0])
        # Fetch the exact ref; it can advance between the advertisement and fetch.
        # No shared FETCH_HEAD, tags, remote-tracking refs or automatic maintenance.
        self.git.require("could not fetch remote plan history", "fetch", "--no-tags", "--no-auto-gc",
                         "--no-recurse-submodules", self.remote, self.ref)
        return self.oid_from(self.git.require("could not read fetched plan tip", "rev-parse", "--verify", "FETCH_HEAD"))

    def commit(self, oid):
        if self.git.require("remote or local plan tip is missing", "cat-file", "-t", oid) != b"commit\n":
            raise Refusal("plan tip must be a commit")
        raw = self.git.require("could not read plan commit", "cat-file", "commit", oid)
        header, _, message = raw.partition(b"\n\n")
        lines = header.split(b"\n")
        tree = self.oid_from(lines[0].removeprefix(b"tree "))
        parents = [self.oid_from(line[7:]) for line in lines[1:] if line.startswith(b"parent ")]
        return tree, parents, message

    def snapshot(self, oid):
        tree, parents, message = self.commit(oid)
        listing = self.git.require("could not inspect plan tree", "ls-tree", "-z", tree)
        metadata, _, name = listing.removesuffix(b"\0").partition(b"\t")
        fields = metadata.split(b" ")
        if len(fields) != 3 or fields[:2] != [b"100644", b"blob"] or not FILENAME.fullmatch(name):
            raise Refusal("plan history must contain one regular Markdown file with a safe name")
        return tree, parents, message, self.oid_from(fields[2]), name.decode("ascii")

    def ancestor(self, old, new):
        if old is None:
            return True
        if new is None:
            return False
        check = self.git.run("merge-base", "--is-ancestor", old, new)
        if check.returncode not in (0, 1):
            raise Refusal("could not compare local and remote plan histories")
        return check.returncode == 0

    def write_candidate(self, blob, filename, parent):
        tree = self.oid_from(self.git.require("could not write plan tree", "mktree",
                                             data=f"100644 blob {blob}\t{filename}\n".encode("ascii")))
        flags = ("-p", parent) if parent is not None else ()
        return self.oid_from(self.git.require("could not write unsigned plan commit", "commit-tree", tree,
                                             *flags, "--no-gpg-sign", data=MESSAGE))

    def push(self, candidate):
        result = self.git.run("push", "--porcelain", "--no-signed", "--no-follow-tags", "--recurse-submodules=no",
                              self.remote, f"{candidate}:{self.ref}")
        if not result.returncode:
            return True
        # Only known ref-contention statuses merit a retry, and only if a
        # subsequent lookup confirms the remote tip has actually advanced.
        prefix = b"!\t" + candidate.encode("ascii") + b":" + self.ref_bytes + b"\t"
        contention = (b"[rejected] (non-fast-forward)", b"[rejected] (fetch first)",
                      b"[remote rejected] (failed to update ref)")
        if any(line.startswith(prefix) and line[len(prefix):] in contention for line in result.stdout.split(b"\n")):
            return False
        raise Refusal("plan push failed; check the GitHub token and remote ref permissions")


def local_tip(local, history):
    for ref in (history.ref, "HEAD"):
        result = local.run("symbolic-ref", "--quiet", ref)
        if result.returncode not in (0, 1):
            raise Refusal("could not inspect local ref")
        if not result.returncode and (ref == history.ref or result.stdout == history.ref_bytes + b"\n"):
            raise Refusal("local plan ref must not be symbolic or checked out")
    result = local.run("show-ref", "--verify", "--hash", history.ref)
    # --verify without --quiet uses status 128 for a missing ref. With --quiet
    # Git distinguishes the missing ref from e.g. a corrupt referenced object.
    if result.returncode == 128 and local.run("show-ref", "--verify", "--quiet", history.ref).returncode == 1:
        return None
    if result.returncode:
        raise Refusal("could not read local plan ref")
    return history.oid_from(result.stdout)


def choose_candidate(history, old, parent, blob, first_filename, data):
    if not history.ancestor(old, parent):
        _, parents, message, old_blob, _ = history.snapshot(old)
        # Recover an interrupted local candidate only for these checked bytes
        # and a compatible base. Don't publish unverified local commit metadata.
        if old_blob != blob or message != MESSAGE or len(parents) > 1 or (parents and not history.ancestor(parents[0], parent)):
            raise Refusal("local-only plan history conflicts with the remote; local ref left intact")
    filename = first_filename
    if parent is not None:
        _, _, message, parent_blob, filename = history.snapshot(parent)
        if parent_blob == blob and message == MESSAGE:
            return parent, filename, True
    written = history.git.require("could not write exact plan bytes", "hash-object",
                                  "-w", "--no-filters", "--stdin", data=data)
    if history.oid_from(written) != blob:
        raise Refusal("written plan blob did not match checked bytes")
    return history.write_candidate(blob, filename, parent), filename, False


def publish(local, history, branch, url, data):
    old = local_tip(local, history)
    parent = history.remote_tip()
    blob = history.oid_from(history.git.require("could not hash exact plan bytes", "hash-object",
                                                "--no-filters", "--stdin", data=data))
    slug = re.sub(r"[^a-z0-9]+", "-", branch.lower(), flags=re.ASCII).strip("-")[:64].rstrip("-") or "plan"
    filename = f"{datetime.now(timezone.utc):%Y-%m-%d}-{slug}.md"
    for attempt in range(MAX_ATTEMPTS):
        candidate, file, reused = choose_candidate(history, old, parent, blob, filename, data)
        local.require("local plan ref changed during publication; concurrent ref left intact", "update-ref",
                      "--no-deref", history.ref, candidate, old or history.zero)
        # Never resolve the mutable local ref again for this push.
        if history.push(candidate):
            report({"sha": candidate, "file": file, "ref": history.ref,
                    "plan_url": f"{url}/blob/{candidate}/{file}", "reused": reused})
            return
        if attempt + 1 == MAX_ATTEMPTS:
            break
        winner = history.remote_tip()
        if winner == parent or winner is None or not history.ancestor(parent, winner):
            raise Refusal("plan push rejected without a fast-forward remote winner; local candidate retained")
        old, parent = candidate, winner
    raise Refusal("remote changed too often; retry plan publication with the same snapshot bytes")


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise Refusal("invalid arguments; see publish-plan.sh --help")


def main():
    parser = Parser(prog="publish-plan.sh", allow_abbrev=False,
                    description="Publish UTF-8 Markdown to refs/plans/<branch> from a delivery worktree.")
    parser.add_argument("--source", required=True, help="regular file in the worktree, /tmp, or /var/tmp")
    parser.add_argument("--branch", required=True, help="short delivery branch name")
    args = parser.parse_args()
    local = local_git()
    ref = f"refs/plans/{args.branch}"
    if not args.branch or args.branch.startswith(("-", "refs/")) or local.run("check-ref-format", ref).returncode:
        raise Refusal("invalid delivery branch")
    repo = destination(local)
    root = local.require("run from a Git worktree", "rev-parse", "--show-toplevel")
    data = read_source(source_path(args.source, Path(os.fsdecode(root.removesuffix(b"\n")))))
    # Scan the same captured bytes passed to Git below, before any object or ref writes.
    require_clean(data)
    token = gh_token()
    require_visibility(repo, token)
    url = f"https://github.com/{repo}"
    # GitHub's Git transport uses SHA-1. Don't mix object formats in the same store.
    if local.require("could not determine Git object format", "rev-parse", "--show-object-format") != b"sha1\n":
        raise Refusal("plan publishing requires a GitHub-compatible SHA-1 checkout")
    objects = os.fsdecode(local.require("could not locate Git object directory", "rev-parse",
                                        "--path-format=absolute", "--git-path", "objects").removesuffix(b"\n"))
    with tempfile.TemporaryDirectory(prefix="pantheon-publish-plan-", dir="/tmp") as temp:
        # Share objects, not refs/config/index, with the worktree. The transport
        # cannot inherit URL rewrites, credential helpers, mirror mode or push
        # options. The token rides along as a Git extraheader, the same basic
        # auth the gh CLI uses for Git over HTTPS; ambient egress configuration
        # (proxy, CA bundle) travels as environment, never as config.
        extraheader = "Authorization: Basic " + base64.b64encode(
            f"x-access-token:{token}".encode("utf-8")).decode("ascii")
        env = dict(GIT_ENV, **IDENTITY, HOME=temp, XDG_CONFIG_HOME=temp, GIT_CONFIG_NOSYSTEM="1",
                   GIT_CONFIG_GLOBAL=os.devnull, GIT_ALLOW_PROTOCOL="https", GIT_CONFIG_COUNT="2",
                   GIT_CONFIG_KEY_0="http.extraheader", GIT_CONFIG_VALUE_0=extraheader,
                   GIT_CONFIG_KEY_1="http.followRedirects", GIT_CONFIG_VALUE_1="false",
                   **egress_env())
        git = Git(temp, env)
        git.require("could not initialize isolated Git transport", "init", "--bare", "--template=", "--object-format=sha1")
        git.env["GIT_OBJECT_DIRECTORY"] = objects
        publish(local, History(git, url + ".git", ref, 40), args.branch, url, data)


try:
    os.umask(0o077)
    main()
except Refusal as error:
    print(f"publish-plan: {error}", file=sys.stderr)
    sys.exit(1)
except (OSError, ValueError, UnicodeError):
    # Raw exceptions from paths, user input, or subprocesses can expose values.
    print("publish-plan: could not process snapshot or Git state", file=sys.stderr)
    sys.exit(1)
PYTHON
