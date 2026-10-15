#!/usr/bin/env python3
"""Probe reasoning summaries and tool-result replay with real credentials.

Usage (from checkout root, OUTSIDE the agent sandbox):
    cargo build --locked -p harnx -p harnx-worker --bin harnx --bin harnx-worker
    python3 scripts/probe_reasoning_summaries.py
    python3 scripts/probe_reasoning_summaries.py --dry-run
    python3 scripts/probe_reasoning_summaries.py openai:gpt-5.6-sol codex:gpt-5.6-sol:high
    python3 scripts/probe_reasoning_summaries.py bedrock-converse:us.anthropic.claude-sonnet-5-5:thinking
    python3 scripts/probe_reasoning_summaries.py --reanalyze probe-report-20261014T072233342919Z

The script rebuilds both binaries before probing (Cargo normally reuses the build
above), then uses Cargo's executable artifacts, never installed binaries. Optional
HARNX_BIN/HARNX_WORKER_BIN overrides must match those freshly built artifacts.
CARGO_TARGET_DIR is supported. Requires Python 3.10+, Cargo, normal harnx client
config, a local NATS broker (harnx normally starts it), and provider credentials.
The bedrock client must use type: bedrock (Converse), not openai-compatible.
HARNX_NATS_SERVER is cleared for this probe so a shared worker cannot run it.
Sources ~/.local/share/harnx/.env if present; AWS's normal credential chain stays
intact. --dry-run does not source credentials, build, write files, or use network.

Each model gets a new local session and temporary tool-restricted agent in the
normal config directory. One prompt calls the read-only built-in metadata tool;
the worker's automatic tool-result continuation is provider turn 2. No external
tool binaries are needed. Temporary agent files are removed, sessions remain.

Writes probe-report-<UTC timestamp>.md and private trace/log artifacts beside it.
Auth is redacted before retaining artifacts; trace bodies still contain private
session metadata and opaque reasoning signatures. Inspect before sharing. No raw
provider output is printed. Report timings are request-to-last-event wall time,
not separate CLI process timings. Missing thought text is reported, not treated
as replay failure. Exit 1 if any model's tool-result continuation isn't accepted.
--reanalyze prints revised Markdown to stdout using existing traces plus recorded
exit codes/status from the sibling .md report. It never sources credentials,
builds, uses network, or writes files. Auxiliary calls (e.g. titles) are listed
but excluded from verdicts. Ambiguous events without request IDs fail closed.
"""

import argparse
import datetime as dt
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import time
from urllib.parse import parse_qsl, unquote, urlencode, urlsplit, urlunsplit
import uuid

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MODELS = [
    "openai:gpt-5.6-sol",
    "codex:gpt-5.6-sol:high",
    "gemini:gemini-3.8-flash",
    "bedrock:us.anthropic.claude-sonnet-4-6:thinking",
    "bedrock:us.amazon.nova-2-lite-v1:0",
]
TOOL = "harnx_read_session_meta"
PROMPT = (
    "This is a reasoning-summary and tool-replay probe. First reason about how "
    "to verify that you are continuing the same session after a tool result. "
    "Then call harnx_read_session_meta exactly once with {} before answering. "
    "After its result, do not call any more tools. Confirm that the result belongs "
    "to this same session and answer briefly. Do not reveal credential values."
)
AUTH_KEY = re.compile(
    r"authorization|proxy.authorization|api.?key|access.?key|secret.?key|"
    r"session.?token|security.?token|access.?token|password|credential|bearer|"
    r"^token$|^key$|x-amz-signature", re.I
)
SECRET_ENV = re.compile(r"API.?KEY|TOKEN|PASSWORD|SECRET|CREDENTIAL|ACCESS_KEY", re.I)
BUILD = ["cargo", "build", "--locked", "-p", "harnx", "-p", "harnx-worker",
         "--bin", "harnx", "--bin", "harnx-worker", "--message-format=json"]


class Redactor:
    def __init__(self, env):
        self.secrets = sorted(
            {value for key, value in env.items() if SECRET_ENV.search(key) and len(value) >= 4},
            key=len, reverse=True,
        )

    def text(self, value):
        for secret in self.secrets:
            value = value.replace(secret, "[REDACTED]")
        value = re.sub(r"(?i)(bearer\s+)[^\s\"',;]+", r"\1[REDACTED]", value)
        value = re.sub(
            r"(?i)((?:authorization|x-api-key|api_key|access_token|secret_access_key)"
            r"[\"']?\s*[:=]\s*[\"']?)[^\s\"',;&}]+", r"\1[REDACTED]", value,
        )
        value = re.sub(r"https?://[^\s<>\"']+", self._url_match, value)
        return value

    @staticmethod
    def _url_match(match):
        try:
            url = urlsplit(match.group())
            netloc = url.netloc.rsplit("@", 1)[-1] if "@" in url.netloc else url.netloc
            query = urlencode([(key, "[REDACTED]" if AUTH_KEY.search(key) else value)
                               for key, value in parse_qsl(url.query, keep_blank_values=True)])
            return urlunsplit((url.scheme, netloc, url.path, query, url.fragment))
        except ValueError:
            return "[REDACTED URL]"

    def obj(self, value):
        if isinstance(value, dict):
            return {key: "[REDACTED]" if AUTH_KEY.search(key) else self.obj(item)
                    for key, item in value.items()}
        if isinstance(value, list):
            return [self.obj(item) for item in value]
        return self.text(value) if isinstance(value, str) else value


def credential_env():
    env = dict(os.environ)
    dotenv = Path.home() / ".local/share/harnx/.env"
    if dotenv.exists():
        # Capture exports in memory, never display shell output or enable xtrace.
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", "-c",
             'set +x; set -a; source "$1" >/dev/null 2>&1 || exit; env -0',
             "probe", str(dotenv)], env=env, capture_output=True,
        )
        if result.returncode:
            raise RuntimeError("Could not source ~/.local/share/harnx/.env (output suppressed)")
        env = dict(item.decode().split("=", 1) for item in result.stdout.split(b"\0") if item)
    return env


def config_dir(env):
    if env.get("HARNX_CONFIG_FILE"):
        return Path(env["HARNX_CONFIG_FILE"]).resolve().parent
    if env.get("HARNX_CONFIG_DIR"):
        return Path(env["HARNX_CONFIG_DIR"]).resolve()
    if env.get("XDG_CONFIG_HOME"):
        return Path(env["XDG_CONFIG_HOME"]) / "harnx"
    if sys.platform == "darwin":
        return Path.home() / "Library/Application Support/harnx"
    return Path.home() / ".config/harnx"


def field(value, *path, default=None):
    """Read nested wire fields without treating nulls or scalars as objects."""
    for key in path:
        if not isinstance(value, dict):
            return default
        value = value.get(key, default)
    return value


def object_list(value):
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, dict)]


def nodes(value):
    yield value
    children = ()
    if isinstance(value, dict):
        children = value.values()
    elif isinstance(value, list):
        children = value
    for item in children:
        yield from nodes(item)


def object_nodes(value):
    return (item for item in nodes(value) if isinstance(item, dict))


def has_tool_result(body):
    return any(
        item.get("type") == "function_call_output" or item.get("role") == "tool"
        or "functionResponse" in item or "toolResult" in item
        for item in object_nodes(body)
    )


SUMMARY_FIELDS = (
    (("reasoning",), "summary", "reasoning.summary"),
    (("additionalModelRequestFields", "thinking"), "display", "thinking.display"),
    (("generationConfig", "thinkingConfig"), "includeThoughts", "includeThoughts"),
)


def summary_setting(body):
    for path, key, label in SUMMARY_FIELDS:
        setting = field(body, *path, default={})
        if not isinstance(setting, dict):
            continue
        if key in setting:
            return label + "=" + json.dumps(setting[key])
    return "absent (non-Anthropic Bedrock may have no summary switch)"


def gemini_candidates(body):
    return object_list(field(body, "candidates"))


def gemini_thoughts(body):
    for candidate in gemini_candidates(body):
        for part in object_list(field(candidate, "content", "parts")):
            if part.get("thought") is True:
                yield part.get("text")


def streamed_thoughts(entry):
    body = field(entry, "body")
    if field(body, "type") in ("response.reasoning_summary_text.delta", "response.reasoning_text.delta"):
        yield field(body, "delta")
        return
    if field(entry, "provider") in ("gemini", "vertexai"):
        yield from gemini_thoughts(body)
        return
    if field(entry, "provider") != "bedrock":
        return
    if field(body, "smithy_type") == "contentBlockDelta":
        yield field(body, "event", "delta", "reasoningContent", "text")


def thought_chars(entry):
    if field(entry, "kind") != "stream-event":
        return 0
    return sum(len(text.strip()) for text in streamed_thoughts(entry) if isinstance(text, str))


def bedrock_completed(body):
    if field(body, "smithy_type") != "messageStop":
        return False
    return field(body, "event", "stopReason") in ("end_turn", "tool_use")


def openai_completed(body):
    response = field(body, "response")
    if not isinstance(response, dict):
        return False
    return response.get("status", "completed") == "completed"


def completed(entry):
    if field(entry, "kind") != "stream-event":
        return False
    body = field(entry, "body")
    if field(body, "type") == "response.completed":
        return openai_completed(body)
    if field(entry, "provider") in ("gemini", "vertexai"):
        return any(candidate.get("finishReason") == "STOP" for candidate in gemini_candidates(body))
    if field(entry, "provider") == "bedrock":
        return bedrock_completed(body)
    return False


def exposed_usage(body):
    for item in object_nodes(body):
        for key in ("usage", "usageMetadata"):
            usage = item.get(key)
            if isinstance(usage, dict):
                yield usage


def usage_objects(entries):
    found = []
    for entry in entries:
        for usage in exposed_usage(field(entry, "body")):
            if usage not in found:
                found.append(usage)
    return found


def seconds(start, end):
    try:
        return f"{(dt.datetime.fromisoformat(end.replace('Z', '+00:00')) - dt.datetime.fromisoformat(start.replace('Z', '+00:00'))).total_seconds():.3f}s"
    except (ValueError, TypeError, AttributeError):
        return "unavailable"


def request_identity(entry):
    """Trace requests have no provider field; infer protocol/model from endpoint."""
    url = urlsplit(first_text(entry, (("url",),)))
    path = unquote(url.path)
    model = first_text(entry, (("body", "model"),))
    provider = ""
    if re.search(r"/models/[^/]+:(?:streamGenerateContent|generateContent)$", path):
        provider = "vertexai"  # Gemini uses this trace provider too.
        model = path.rsplit("/models/", 1)[1].rsplit(":", 1)[0]
    elif re.search(r"/model/.+/converse(?:-stream)?$", path):
        provider = "bedrock"
        model = path.split("/model/", 1)[1].rsplit("/", 1)[0]
    elif path.endswith("/responses"):
        provider = "openai_responses"
    elif path.endswith("/chat/completions"):
        provider = "openai"
    return provider, model, url.hostname or "", path


def first_text(value, paths):
    """Identity fields differ by protocol; only nonempty strings identify calls."""
    for path in paths:
        text = field(value, *path)
        if not isinstance(text, str):
            continue
        if text:
            return text
    return ""


def event_identity(entry):
    body = field(entry, "body")
    provider = first_text(entry, (("provider",),))
    if provider == "gemini":
        provider = "vertexai"
    model = first_text(body, (("modelVersion",), ("model",), ("response", "model")))
    response_id = first_text(body, (("responseId",), ("response", "id"), ("response_id",)))
    return provider, model, response_id


def matches_event(attempt, provider, model):
    protocol, requested_model, _, _ = attempt["identity"]
    if protocol and protocol != provider:
        return False
    # Versioned response model IDs may add a snapshot suffix to the request ID.
    return not model or not requested_model or model == requested_model or model.startswith(requested_model + "-")


def probe_request(body):
    tools = field(body, "tools") or field(body, "toolConfig", "tools")
    declared = any(item.get("name") == TOOL for item in object_nodes(tools))
    return declared or has_tool_result(body)


def trailing_bedrock_usage(entry):
    return (field(entry, "provider"), field(entry, "body", "smithy_type")) == ("bedrock", "metadata")


class AttemptLedger:
    """Attribute wire events without allowing auxiliary calls to prove replay."""

    def __init__(self):
        self.attempts = []
        self.by_response_id = {}
        self.unassigned = 0

    def add_request(self, entry):
        relevant = probe_request(field(entry, "body"))
        identity = request_identity(entry)
        if relevant:
            self.close_previous(identity)
        self.attempts.append({"request": entry, "events": [], "relevant": relevant,
                              "identity": identity, "closed": False})

    def close_previous(self, identity):
        # Probe rounds are sequential, even when an earlier round failed.
        for previous in self.attempts:
            if not previous["relevant"]:
                continue
            if previous["identity"] == identity:
                previous["closed"] = True

    def candidates(self, entry, identity):
        provider, model, response_id = identity
        bound = self.by_response_id.get((provider, response_id))
        if bound is not None:
            return [bound]
        matching = [attempt for attempt in self.attempts if matches_event(attempt, provider, model)]
        active = [attempt for attempt in matching if not attempt["closed"]]
        if active:
            return active
        if trailing_bedrock_usage(entry):
            # Converse usage arrives after messageStop, without a response ID.
            return matching[-1:]
        return []

    def add_event(self, entry):
        identity = event_identity(entry)
        candidates = self.candidates(entry, identity)
        if len(candidates) != 1:
            # No request IDs in this trace schema. Same-model title output is
            # ambiguous until a response ID has been bound to one request.
            self.unassigned += 1
            return
        attempt = candidates[0]
        attempt["events"].append(entry)
        provider, _, response_id = identity
        if response_id:
            self.by_response_id[(provider, response_id)] = attempt
        if completed(entry) or field(entry, "kind") == "response":
            attempt["closed"] = True


def attribute_events(entries):
    ledger = AttemptLedger()
    for entry in entries:
        kind = field(entry, "kind")
        if kind == "request":
            ledger.add_request(entry)
        elif kind in ("stream-event", "response"):
            ledger.add_event(entry)
    return ledger.attempts, ledger.unassigned


def bedrock_client_problem(attempts, model):
    client = (model or "").split(":", 1)[0].rsplit("/", 1)[-1]
    for attempt in attempts:
        provider, _, host, path = attempt["identity"]
        is_bedrock = client.startswith("bedrock") or bool(re.search(r"(?:^|\.)bedrock-runtime[.-]", host))
        if is_bedrock and provider != "bedrock":
            return "client is not type: bedrock (Converse); request endpoint is " + path
    return ""


def replay_completed(replay):
    if not replay:
        return False
    return all(any(completed(event) for event in attempt["events"]) for attempt in replay)


def process_status(output):
    statuses = re.findall(r"(?i)(?:HTTP(?:\s+status|\s+error)?|status(?:\s+code)?)[\s:=]+([1-5]\d\d)\b|\b([45]\d\d)\s+(?:Bad Request|Unauthorized|Forbidden|Not Found|Internal Server Error)", output)
    return ", ".join(dict.fromkeys(a or b for a, b in statuses)) or "status unavailable"


def first_events(attempts):
    return attempts[0]["events"] if attempts else []


def partition_attempts(all_attempts):
    attempts, auxiliary = [], []
    for attempt in all_attempts:
        if attempt["relevant"]:
            attempts.append(attempt)
        else:
            auxiliary.append(attempt)
    return attempts, auxiliary


def analyze(entries, returncode, output, model=None):
    all_attempts, unassigned = attribute_events(entries)
    attempts, auxiliary = partition_attempts(all_attempts)
    problem = bedrock_client_problem(attempts, model)
    replay = [attempt for attempt in attempts if has_tool_result(field(attempt["request"], "body"))]
    accepted = not problem and returncode == 0 and replay_completed(replay)
    thoughts = sum(thought_chars(event) for event in first_events(attempts))
    return {"attempts": attempts, "auxiliary": auxiliary, "unassigned": unassigned,
            "replay": replay, "accepted": bool(accepted), "problem": problem,
            "thoughts": thoughts, "status": process_status(output)}


def verdict(evidence):
    return evidence["problem"] or ("accepted" if evidence["accepted"] else "FAILED / not demonstrated")


def duration(attempt):
    if attempt and attempt["events"]:
        return seconds(attempt["request"].get("ts"), attempt["events"][-1].get("ts"))
    return "unavailable"


def auxiliary_details(evidence):
    details = [f"- Auxiliary requests ignored for verdict: {len(evidence['auxiliary'])}; unassigned events: {evidence['unassigned']}."]
    for attempt in evidence["auxiliary"]:
        provider, model, host, path = attempt["identity"]
        details.append(f"- Auxiliary: {provider or 'unknown protocol'}, `{model}`, `{host}{path}`, {duration(attempt)}.")
    return details


@dataclass(frozen=True)
class Invocation:
    agent: str
    session: str
    model: str
    timeout: int


def command(binary, invocation):
    return [str(binary), "prompt", "--agent", invocation.agent, "--session", invocation.session,
            "--model", invocation.model, "--timeout-secs", str(invocation.timeout), "--", PROMPT]


def cargo_binaries(output):
    bins = {}
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if field(item, "reason") != "compiler-artifact":
            continue
        name = first_text(item, (("target", "name"),))
        if not name:
            continue
        executable = first_text(item, (("executable",),))
        if executable:
            bins[name] = Path(executable).resolve()
    return bins


def validate_binaries(bins, env):
    for name, variable in (("harnx", "HARNX_BIN"), ("harnx-worker", "HARNX_WORKER_BIN")):
        if name not in bins:
            raise RuntimeError(f"Cargo did not produce a native {name} executable")
        override = env.get(variable)
        if not override:
            continue
        if Path(override).resolve() != bins[name]:
            raise RuntimeError(f"{variable} does not match freshly built artifact; unset it")


def build_binaries(env, artifacts, redact):
    result = subprocess.run(BUILD, cwd=ROOT, env=env, capture_output=True, text=True)
    (artifacts / "build.log").write_text(redact.text(result.stderr))
    if result.returncode:
        raise RuntimeError("Build failed; inspect private build.log")
    bins = cargo_binaries(result.stdout)
    validate_binaries(bins, env)
    return bins


def private_write(path, text):
    path.write_text(text)
    path.chmod(0o600)


def parse_trace_lines(lines):
    entries, invalid = [], 0
    for line in lines:
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            invalid += 1
            continue
        if not isinstance(entry, dict):
            invalid += 1
            continue
        entries.append(entry)
    return entries, invalid


def sanitized_trace(trace, redact):
    entries, invalid = [], 0
    if trace.exists():
        entries, invalid = parse_trace_lines(trace.read_text(errors="replace").splitlines())
    # Sanitize disk before analysis can fail; keep unredacted evidence in memory.
    private_write(trace, "".join(json.dumps(redact.obj(entry)) + "\n" for entry in entries))
    return entries, invalid


def process_text(value):
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return value or ""


def run_command(cmd, env, timeout):
    start = time.monotonic()
    try:
        result = subprocess.run(cmd, cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, errors="replace",
                                timeout=timeout + 60)
    except subprocess.TimeoutExpired as error:
        # CLI timeout covers turns; this bounds startup/shutdown hangs too.
        result = subprocess.CompletedProcess(cmd, 124, process_text(error.stdout),
            process_text(error.stderr) + "\nProbe observer timeout: CLI did not exit within allowance + 60s")
    return result, time.monotonic() - start


def isolate_worker(env, bins):
    env["HARNX_WORKER_BIN"] = str(bins["harnx-worker"])
    # Do not inherit an orchestrator's active session/agent targeting context.
    for key in ("HARNX_SESSION_ID", "HARNX_AGENT_NAME", "HARNX_CLUSTER", "HARNX_LLM_TRACE"):
        env.pop(key, None)
    # Empty ambient value also blocks harnx's own .env loader from restoring it.
    env["HARNX_NATS_SERVER"] = ""


def evidence_times(evidence):
    attempts = evidence["attempts"]
    t1 = duration(attempts[0]) if attempts else "unavailable"
    t2 = ", ".join(duration(attempt) for attempt in evidence["replay"]) or "unavailable"
    return t1, t2


def usage_summary(evidence):
    initial_usage = usage_objects(first_events(evidence["attempts"]))
    replay_usage = [usage_objects(attempt["events"])[-1:] for attempt in evidence["replay"]]
    return "T1: " + json.dumps(initial_usage[-1:]) + "; T2: " + json.dumps(replay_usage)


def live_row(model, evidence, returncode):
    attempts = evidence["attempts"]
    summary = summary_setting(field(attempts[0]["request"], "body")) if attempts else "request unavailable"
    t1, t2 = evidence_times(evidence)
    usage = usage_summary(evidence)
    return f"| {model} | {summary} | {evidence['thoughts']} chars | {verdict(evidence)}; {evidence['status']}; exit {returncode} | {t1} | {t2} | `{usage}` |"


def live_attempt_details(attempts):
    details = []
    for number, attempt in enumerate(attempts, 1):
        body = field(attempt["request"], "body")
        kind = "tool-result replay" if has_tool_result(body) else "initial"
        details.append(f"- Attempt {number}: {kind}, {duration(attempt)}, {summary_setting(body)}; usage: `" + json.dumps(usage_objects(attempt["events"])) + "`.")
    return details


@dataclass
class Observation:
    invocation: Invocation
    trace: Path
    log: Path
    result: subprocess.CompletedProcess
    elapsed: float
    output: str
    invalid: int
    evidence: dict

    def details(self):
        evidence = self.evidence
        invocation = self.invocation
        details = [f"\n## {invocation.model}\n",
                   f"- CLI wall time: {self.elapsed:.3f}s; provider attempts: {len(evidence['attempts'])}; tool-result attempts: {len(evidence['replay'])}.",
                   f"- Trace: `{self.trace}`", f"- Process output (redacted): `{self.log}`",
                   f"- Session: `{invocation.agent} / {invocation.session}` (local); invalid trace lines: {self.invalid}."]
        details.extend(live_attempt_details(evidence["attempts"]))
        details.extend(auxiliary_details(evidence))
        if not evidence["accepted"]:
            details.extend(["\nProcess output excerpt (HTTP status is not inferred):\n", "```text", self.output[-2500:].replace("```", "'''"), "```"])
        return details


def report_lines(stamp, bins, rows, sections):
    return ["# Reasoning-summary probe\n", f"UTC run: {stamp}\n",
            f"Frontend: `{bins['harnx']}`; worker: `{bins['harnx-worker']}`. Both built by this script.\n",
            "Turns are provider requests within one CLI prompt. Every tool-result attempt must have a successful terminal streaming event, and CLI must exit 0. A successful retry doesn't hide a failed replay attempt. Missing tool calls, non-streaming responses, and absent traces fail closed. HTTP status comes only from process output, never from trace or exit 0.\n",
            "Thought count covers non-empty streamed wire reasoning text in turn 1, excluding signatures and replayed text. It does not prove frontend ThoughtChunk rendering. Summary values come from actual request bodies after patches. Non-Anthropic Bedrock may have no summary switch.\n",
            "Timings use request and last event timestamps (include network; exclude CLI startup/tool execution). Multiple tool-result attempts have separate times; retries are not hidden. Usage below is provider-native, may be cumulative snapshots, and is not summed. Empty usage list means unavailable. Artifacts are private but still contain session metadata and opaque reasoning signatures; inspect before sharing.\n",
            "| Model | Turn 1 summary setting | Turn 1 streamed reasoning | Turn 2 result / HTTP status / process | Turn 1 wall | Turn 2 wall (each attempt) | Usage (last exposed snapshot per attempt) |",
            "|---|---|---|---|---|---|---|", *rows, *sections]


class ProbeRunner:
    """Own one run's private artifacts and temporary agent, including cleanup."""

    def __init__(self, args):
        self.args = args
        self.stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
        self.agent = "reasoning-probe-" + uuid.uuid4().hex
        self.report = args.output_dir.resolve() / f"probe-report-{self.stamp}.md"
        self.artifacts = self.report.with_suffix("")
        self.models = args.models or DEFAULT_MODELS
        self.created_agent = False

    def invocation(self, index, model):
        return Invocation(self.agent, f"{self.agent}-{index}", model, self.args.timeout_secs)

    def dry_run(self):
        print("No credentials loaded; no build, files, or network. Planned commands:")
        print("cd " + shlex.quote(str(ROOT)))
        print(shlex.join(BUILD) + "  # executable paths read from Cargo artifacts")
        print('Source ~/.local/share/harnx/.env if present (exports captured, never printed).')
        print(f"Create temporary read-only-tool agent {self.agent} in normal config; remove after probe.")
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        for index, model in enumerate(self.models, 1):
            trace = self.artifacts / f"{index:02d}.trace.jsonl"
            cmd = command(target / "debug/harnx", self.invocation(index, model))
            print(shlex.join(["env", "HARNX_NATS_SERVER=", f"HARNX_WORKER_BIN={target / 'debug/harnx-worker'}",
                              f"HARNX_LLM_TRACE={trace}"] + cmd))
        print(f"Redact artifacts; write {self.report}; exit non-zero if any tool-result turn 2 fails.")
        return 0

    def prepare(self):
        os.umask(0o077)
        self.env = credential_env()
        self.redact = Redactor(self.env)
        self.args.output_dir.mkdir(parents=True, exist_ok=True)
        self.artifacts.mkdir(mode=0o700)
        self.bins = build_binaries(self.env, self.artifacts, self.redact)
        isolate_worker(self.env, self.bins)
        self.agent_file = config_dir(self.env) / "agents" / f"{self.agent}.md"
        self.agent_file.parent.mkdir(parents=True, exist_ok=True)

    def write_agent(self, index, model):
        # Exclusive creation prevents deleting a pre-existing agent on collision.
        with self.agent_file.open("w" if index > 1 else "x") as handle:
            self.created_agent = True
            handle.write("---\nmodel: " + json.dumps(model) + "\nuse_tools:\n  - " + TOOL
                         + "\n---\n" + PROMPT + "\n")

    def probe_model(self, index, model):
        self.write_agent(index, model)
        invocation = self.invocation(index, model)
        trace = self.artifacts / f"{index:02d}.trace.jsonl"
        log = self.artifacts / f"{index:02d}.process.log"
        self.env["HARNX_LLM_TRACE"] = str(trace)
        result, elapsed = run_command(command(self.bins["harnx"], invocation), self.env, invocation.timeout)
        output = self.redact.text(result.stdout + "\n" + result.stderr)
        private_write(log, output)
        entries, invalid = sanitized_trace(trace, self.redact)
        evidence = analyze(entries, result.returncode, self.redact.text(result.stderr), model)
        return Observation(invocation=invocation, trace=trace, log=log, result=result,
                           elapsed=elapsed, output=output, invalid=invalid, evidence=evidence)

    def run(self):
        self.prepare()
        rows, sections = [], []
        failed = False
        try:
            for index, model in enumerate(self.models, 1):
                observation = self.probe_model(index, model)
                evidence = observation.evidence
                failed |= not evidence["accepted"] or bool(observation.invalid)
                rows.append(live_row(model, evidence, observation.result.returncode))
                sections.extend(observation.details())
                print(f"{model}: {verdict(evidence)}; thought chars {evidence['thoughts']}; exit {observation.result.returncode}")
        finally:
            if self.created_agent:
                self.agent_file.unlink(missing_ok=True)
        header = report_lines(self.stamp, self.bins, rows, sections)
        private_write(self.report, self.redact.text("\n".join(header) + "\n"))
        print(f"Report: {self.report}")
        return int(failed)


def run_probe(args):
    runner = ProbeRunner(args)
    return runner.dry_run() if args.dry_run else runner.run()


def report_metadata_row(line):
    if not line.startswith("| "):
        return None
    cells = [cell.strip() for cell in line.split("|")[1:-1]]
    if len(cells) < 4:
        return None
    if ":" not in cells[0]:
        return None
    exit_code = re.search(r"\bexit (\d+)\b", cells[3])
    status = re.search(r"; ([1-5]\d\d(?:, [1-5]\d\d)*); exit", cells[3])
    return (cells[0], int(exit_code[1]) if exit_code else None,
            "HTTP " + status[1] if status else "")


def report_metadata(original):
    metadata = []
    for line in original.read_text().splitlines():
        row = report_metadata_row(line)
        if row is not None:
            metadata.append(row)
    return metadata


def offline_trace(trace):
    lines = (line for line in trace.read_text().splitlines() if line.strip())
    entries, invalid = parse_trace_lines(lines)
    if invalid:
        raise RuntimeError("Invalid trace entries; refusing incomplete offline evidence")
    return entries


def offline_row(model, evidence, returncode):
    t1, t2 = evidence_times(evidence)
    exit_code = returncode if returncode is not None else "unavailable"
    return f"| {model} | {verdict(evidence)}; {evidence['status']}; exit {exit_code} | {evidence['thoughts']} | {t1} | {t2} |"


def offline_details(model, trace, evidence):
    details = [f"\n## {model}\n", f"- Trace (read-only): `{trace}`"]
    for number, attempt in enumerate(evidence["attempts"], 1):
        body = field(attempt["request"], "body")
        details.append(f"- Attempt {number}: {summary_setting(body)}; usage: `" + json.dumps(usage_objects(attempt["events"])) + "`.")
    details.extend(auxiliary_details(evidence))
    return details


def validate_trace_indices(indices):
    if len(set(indices)) != len(indices):
        raise RuntimeError("Trace filenames have duplicate numeric invocation indices; refusing to guess model/exit codes")
    if sorted(indices) != list(range(1, len(indices) + 1)):
        raise RuntimeError("Trace filenames must use contiguous one-based invocation indices; refusing to guess model/exit codes")


def numeric_trace_files(directory):
    numbered = []
    for trace in directory.glob("*.trace.jsonl"):
        match = re.fullmatch(r"(\d+)\.trace\.jsonl", trace.name)
        if not match:
            raise RuntimeError("Trace filename is not a numeric invocation index; refusing to guess model/exit codes")
        numbered.append((int(match[1]), trace))
    numbered.sort(key=lambda item: item[0])
    validate_trace_indices([index for index, _ in numbered])
    return [trace for _, trace in numbered]


def reanalyze(directory):
    """Read existing traces and recorded exit codes; never modify artifacts."""
    directory = directory.resolve()
    original = directory.with_suffix(".md")
    metadata = report_metadata(original)
    traces = numeric_trace_files(directory)
    if not metadata or len(metadata) != len(traces):
        raise RuntimeError("Report table and trace count don't match; refusing to guess model/exit codes")
    lines = ["# Offline reasoning-summary reanalysis\n", f"Source (read-only): `{original}`\n",
             "Exit codes and HTTP status are taken from the original report, not inferred from traces. Auxiliary requests are listed below and excluded from verdicts.\n",
             "| Model | Result | Turn 1 reasoning chars | Turn 1 wall | Turn 2 wall |",
             "|---|---|---|---|---|"]
    details, failed = [], False
    for trace, (model, returncode, status_output) in zip(traces, metadata):
        evidence = analyze(offline_trace(trace), returncode, status_output, model)
        failed |= not evidence["accepted"]
        lines.append(offline_row(model, evidence, returncode))
        details.extend(offline_details(model, trace, evidence))
    print(Redactor({}).text("\n".join(lines + details)))
    return int(failed)


def validate_models(models, parser):
    for model in models:
        if not re.fullmatch(r"[A-Za-z0-9_./:@+-]+", model):
            parser.error("models must be client:model identifiers")
        if ":" not in model:
            parser.error("models must be client:model identifiers")


def parse_arguments():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("models", nargs="*", help="client:model names; default covers five catalog entries")
    parser.add_argument("--dry-run", action="store_true", help="print planned commands without reading credentials or network")
    parser.add_argument("--timeout-secs", type=int, default=600, help="CLI invocation timeout per model (default: 600)")
    parser.add_argument("--output-dir", type=Path, default=ROOT, help="report/artifact directory (default: checkout root)")
    parser.add_argument("--reanalyze", type=Path, metavar="REPORT_DIR", help="read-only offline reanalysis using sibling .md report exit codes; prints Markdown")
    args = parser.parse_args()
    if args.reanalyze:
        if any((args.models, args.dry_run)):
            parser.error("--reanalyze cannot be combined with model arguments or --dry-run")
    if args.timeout_secs <= 0:
        parser.error("--timeout-secs must be positive")
    validate_models(args.models, parser)
    return args


def main():
    args = parse_arguments()
    try:
        return reanalyze(args.reanalyze) if args.reanalyze else run_probe(args)
    except (OSError, RuntimeError, json.JSONDecodeError) as error:
        # Only our own errors and OS path errors reach the console, not provider output.
        print(f"Probe setup failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
