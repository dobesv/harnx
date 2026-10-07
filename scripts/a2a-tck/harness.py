"""Isolated real broker/worker/server and test-only OpenAI streaming endpoint."""

import contextlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.request import Request, urlopen


class Llm(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with (self.server.reports / "llm-requests.jsonl").open("a") as output:
            output.write(json.dumps(body) + "\n")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        try:
            for content, finish in [("Hello ", None), ("world", None), (None, "stop")]:
                chunk = {
                    "id": "tck-completion",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "test",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {} if content is None else {"content": content},
                            "finish_reason": finish,
                        }
                    ],
                }
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                self.wfile.flush()
                time.sleep(0.2)
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


def until(callback, processes, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for process in processes:
            if process.poll() is not None:
                raise RuntimeError(f"SUT process exited: {process.args}")
        try:
            result = callback()
            if result:
                return result
        except (OSError, ValueError):
            pass
        time.sleep(0.1)
    raise TimeoutError("SUT readiness deadline; inspect report logs")


def start_llm(reports):
    llm = ThreadingHTTPServer(("127.0.0.1", 0), Llm)
    llm.reports = reports
    threading.Thread(target=llm.serve_forever, daemon=True).start()
    return llm


def prepare_environment(work):
    env = os.environ.copy()
    env["RUST_LOG"] = "harnx_a2a_server=info"
    env["NO_COLOR"] = "1"
    # Don't inherit routing, credentials, or paths from a developer's live setup.
    for key in list(env):
        if key.startswith("HARNX_"):
            del env[key]
    for kind in ("config", "data", "state"):
        directory = work / kind
        directory.mkdir()
        env[f"HARNX_{kind.upper()}_DIR"] = str(directory)
    return env


def write_config(work, llm):
    config = work / "config"
    for directory in ("agents", "clients", "nats_servers", "broker"):
        (config / directory).mkdir()
    (config / "config.yaml").write_text("model: mock:test\nstream: true\nsave: false\n")
    (config / "agents/tck.md").write_text(
        "---\nmodel: mock:test\nname: TCK\ndescription: Deterministic TCK agent\nversion: '1'\n---\nTest agent\n"
    )
    (config / "clients/mock.yaml").write_text(
        f"type: openai-compatible\nname: mock\napi_base: http://127.0.0.1:{llm.server_port}/v1\n"
        "api_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n"
    )
    return config


@dataclass
class HarnessContext:
    work: Path
    reports: Path
    binaries: Path
    env: dict[str, str]
    processes: list[subprocess.Popen]


def spawn(args, log, context):
    reports, env, processes = context.reports, context.env, context.processes
    with (reports / log).open("w") as output:
        process = subprocess.Popen(
            args,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    processes.append(process)
    return process


def interrupted(_sig, _frame):
    raise KeyboardInterrupt


def start_broker(context, config):
    work, env, processes = context.work, context.env, context.processes
    spawn_process = lambda args, log: spawn(args, log, context)
    spawn_process(
        [
            "nats-server",
            "-js",
            "-a",
            "127.0.0.1",
            "-p",
            "-1",
            "-sd",
            str(work / "jetstream"),
            "--ports_file_dir",
            str(config / "broker"),
        ],
        "nats.log",
    )

    def broker_url():
        paths = list((config / "broker").glob("*.ports"))
        return json.loads(paths[0].read_text())["nats"][0] if paths else None

    url = until(broker_url, processes)
    (config / "nats_servers/tck.yaml").write_text(f"url: {json.dumps(url)}\n")
    env["HARNX_NATS_URL"] = url
    env["HARNX_NATS_TOKEN"] = ""


def start_worker_and_server(context, config):
    binaries = context.binaries
    spawn_process = lambda args, log: spawn(args, log, context)
    spawn_process(
        [
            str(binaries / "harnx-worker"),
            "--cluster",
            "tck",
            "--worker-id",
            "tck",
            "--manage-servers",
            "--healthz-addr",
            "127.0.0.1:0",
        ],
        "worker.log",
    )
    spawn_process(
        [
            str(binaries / "harnx-a2a-server"),
            "--config-dir",
            str(config),
            "--cluster",
            "tck",
            "--agent",
            "tck",
            "--port",
            "0",
        ],
        "server.log",
    )


def wait_for_server(reports, processes):
    def endpoint():
        text = (reports / "server.log").read_text()
        match = re.search(r"serving A2A HTTP.*address=([^\s\x1b]+)", text)
        return f"http://{match[1]}/agents/tck" if match else None

    host = until(endpoint, processes)
    until(
        lambda: (
            urlopen(host + "/.well-known/agent-card.json", timeout=2).status == 200
        ),
        processes,
    )
    return host


def smoke_turn(host, reports):
    # Smoke a real turn before attributing conformance failures to the TCK.
    payload = {
        "jsonrpc": "2.0",
        "id": "smoke",
        "method": "SendMessage",
        "params": {
            "message": {
                "role": "ROLE_USER",
                "parts": [{"text": "Complete task"}],
                "messageId": "smoke",
            }
        },
    }
    response = json.load(
        urlopen(
            Request(
                host,
                data=json.dumps(payload).encode(),
                headers={"Content-Type": "application/json", "A2A-Version": "1.0"},
            ),
            timeout=60,
        )
    )
    (reports / "smoke.json").write_text(json.dumps(response, indent=2))
    assert (
        response["result"]["task"]["status"]["state"] == "TASK_STATE_COMPLETED"
    ), response


def run_tck(context, host):
    work, reports, env, processes = (
        context.work, context.reports, context.env, context.processes
    )
    tck = work / "tck"
    # run_tck.py requires its own repository as cwd.
    with (reports / "tck.log").open("w") as output:
        run = subprocess.Popen(
            [
                str(tck / ".venv/bin/python"),
                "run_tck.py",
                "--sut-host",
                host,
                "--transport",
                "jsonrpc",
                "--level",
                "must",
                "-v",
            ],
            cwd=tck,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    processes.append(run)
    code = run.wait(timeout=600)
    for path in (tck / "reports").glob("*"):
        if path.is_file():
            shutil.copy2(path, reports / path.name)
    print((reports / "tck.log").read_text())
    return code


def cleanup(processes, llm, reports):
    for process in reversed(processes):
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGTERM)
    for process in reversed(processes):
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    llm.shutdown()
    llm.server_close()
    (reports / "processes.json").write_text(
        json.dumps(
            [
                {
                    "pid": process.pid,
                    "command": process.args,
                    "returncode": process.returncode,
                }
                for process in processes
            ],
            indent=2,
        )
    )


def main():
    work, reports, binaries = map(Path, sys.argv[1:])
    processes = []
    llm = start_llm(reports)
    env = prepare_environment(work)
    config = write_config(work, llm)
    context = HarnessContext(work, reports, binaries, env, processes)
    signal.signal(signal.SIGTERM, interrupted)
    try:
        start_broker(context, config)
        start_worker_and_server(context, config)
        host = wait_for_server(reports, processes)
        smoke_turn(host, reports)
        return run_tck(context, host)
    finally:
        cleanup(processes, llm, reports)


if __name__ == "__main__":
    sys.exit(main())
