#!/usr/bin/env python3
"""Offline probe tests. Run: python3 scripts/test_probe_reasoning_summaries.py"""

import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

if __package__:
    from . import probe_reasoning_summaries as probe
    from .probe_reasoning_fixtures import (
        cargo_output, concurrent_title_exchange, numbered_exchange,
        openai_exchange, request, stream, write_trace,
    )
else:
    import probe_reasoning_summaries as probe
    from probe_reasoning_fixtures import (
        cargo_output, concurrent_title_exchange, numbered_exchange,
        openai_exchange, request, stream, write_trace,
    )


class ReanalysisTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name) / "probe-report-test"
        self.directory.mkdir()
        self.report = self.directory.with_suffix(".md")
        self.report.write_text("| Model | Setting | Thoughts | Result |\n")

    def write_invocation(self, index, model, code, entries):
        with self.report.open("a") as report:
            report.write(f"| {model} | auto | 0 | FAILED / not demonstrated; status unavailable; exit {code} |\n")
        trace = self.directory / f"{index:02d}.trace.jsonl"
        write_trace(trace, entries)
        return trace

    def reanalyze(self, expected_code):
        with mock.patch.object(probe, "credential_env", side_effect=AssertionError("credentials")), \
             mock.patch.object(probe.subprocess, "run", side_effect=AssertionError("execution")), \
             contextlib.redirect_stdout(io.StringIO()) as stdout:
            self.assertEqual(probe.reanalyze(self.directory), expected_code)
        return stdout.getvalue()

    def test_reanalyze_is_offline_read_only_and_uses_recorded_exit_codes(self):
        for index, client in enumerate(("openai", "codex"), 1):
            model = client + ":gpt-5.6-sol" + (":high" if client == "codex" else "")
            self.write_invocation(index, model, index - 1, concurrent_title_exchange(client))
        files = [self.report, *self.directory.iterdir()]
        before = {file: file.read_bytes() for file in files}
        output = self.reanalyze(1)
        self.assertIn("openai:gpt-5.6-sol | accepted", output)
        self.assertIn("codex:gpt-5.6-sol:high | FAILED / not demonstrated", output)
        self.assertIn("gemini-3.1-flash-lite", output)
        self.assertEqual(before, {file: file.read_bytes() for file in files})
        self.assertEqual(set(self.directory.iterdir()), set(files) - {self.report})

    def test_reanalyze_pairs_numeric_invocation_indices(self):
        for index in range(1, 101):
            self.write_invocation(index, f"openai:model-{index}", int(index == 100), numbered_exchange(index))
        output = self.reanalyze(1)
        for index in range(1, 101):
            with self.subTest(index=index):
                code = int(index == 100)
                result = "FAILED / not demonstrated" if code else "accepted"
                # Unique reasoning lengths expose metadata paired with another invocation's trace.
                self.assertIn(f"| openai:model-{index} | {result}; status unavailable; exit {code} | "
                              f"{index} | 1.000s | 2.000s |", output)
                self.assertIn(f"## openai:model-{index}\n\n- Trace (read-only): "
                              f"`{self.directory / f'{index:02d}.trace.jsonl'}`", output)
        self.assertLess(output.index("openai:model-11 | accepted"),
                        output.index("openai:model-100 | FAILED / not demonstrated"))

    def test_reanalyze_rejects_gapped_or_non_one_based_trace_indices(self):
        for indices in ((1, 3), (2, 3)):
            with self.subTest(indices=indices):
                with tempfile.TemporaryDirectory() as temporary:
                    self.directory = Path(temporary) / "probe-report-test"
                    self.directory.mkdir()
                    self.report = self.directory.with_suffix(".md")
                    self.report.write_text("| Model | Setting | Thoughts | Result |\n")
                    for index in indices:
                        self.write_invocation(index, f"openai:model-{index}", 0, numbered_exchange(index))
                    with self.assertRaisesRegex(RuntimeError, "contiguous one-based"):
                        probe.reanalyze(self.directory)

    def test_reanalyze_rejects_non_numeric_trace_filename(self):
        trace = self.write_invocation(1, "openai:model", 0, openai_exchange())
        trace.rename(self.directory / "trace.trace.jsonl")
        with self.assertRaisesRegex(RuntimeError, "numeric invocation index"):
            self.reanalyze(1)

    def test_reanalyze_rejects_duplicate_numeric_indices(self):
        trace = self.write_invocation(1, "openai:first", 0, openai_exchange())
        second = self.write_invocation(2, "openai:second", 1, openai_exchange())
        # Equal file/table counts must not hide the ambiguous 1/01 identity.
        second.rename(self.directory / "1.trace.jsonl")
        self.assertTrue(trace.exists())
        with self.assertRaisesRegex(RuntimeError, "duplicate numeric invocation indices"):
            self.reanalyze(1)

    def test_reanalyze_rejects_missing_trace(self):
        self.write_invocation(1, "openai:first", 0, openai_exchange())
        trace = self.write_invocation(2, "openai:second", 1, openai_exchange())
        trace.unlink()
        with self.assertRaisesRegex(RuntimeError, "Report table and trace count don't match"):
            self.reanalyze(1)


class ProbeTests(unittest.TestCase):
    def assert_replay_rejected(self, entries):
        evidence = probe.analyze(entries, 0, "")
        self.assertFalse(evidence["accepted"])
        self.assertEqual(probe.verdict(evidence), "FAILED / not demonstrated")

    def assert_artifacts_redacted(self, directory, secrets):
        for file in directory.rglob("*"):
            if not file.is_file():
                continue
            text = file.read_text()
            for secret in secrets:
                self.assertNotIn(secret, text)
            self.assertEqual(file.stat().st_mode & 0o777, 0o600)

    def test_real_shape_openai_codex_gemini_replay_survives_concurrent_title(self):
        for client in ("openai", "codex", "gemini"):
            with self.subTest(client=client):
                entries = concurrent_title_exchange(client)
                evidence = probe.analyze(entries, 0, "", client + ":model")
                self.assertTrue(evidence["accepted"])
                self.assertEqual(evidence["unassigned"], 0)
                self.assertEqual(len(evidence["attempts"]), 2)
                self.assertEqual(len(evidence["replay"]), 1)
                self.assertEqual(len(evidence["auxiliary"]), 1)
                self.assertEqual(len(evidence["auxiliary"][0]["events"]), 1)
                self.assertIn("gemini-3.1-flash-lite", "\n".join(probe.auxiliary_details(evidence)))
                self.assertEqual(probe.duration(evidence["replay"][0]), "2.000s")
                self.assertEqual(evidence["thoughts"], 0 if client == "openai" else len("reasoning") if client == "gemini" else len("Check session"))
                usage = probe.usage_objects(evidence["replay"][0]["events"])
                self.assertNotIn({"promptTokenCount": 999}, usage)
                # The title's STOP must not count as the missing main completion.
                self.assert_replay_rejected(entries[:-1])

    def test_ambiguous_same_model_auxiliary_cannot_prove_acceptance(self):
        entries = concurrent_title_exchange("gemini")
        entries[3]["url"] = entries[2]["url"]
        entries[4]["body"]["modelVersion"] = "gemini-3.8-flash"
        evidence = probe.analyze(entries, 0, "")
        self.assertFalse(evidence["accepted"])
        self.assertGreater(evidence["unassigned"], 0)

    def test_bedrock_openai_compatible_has_specific_diagnostic_with_custom_prefix(self):
        for client in ("bedrock", "bedrock-converse", "aws-prod"):
            entry = request({"model": "us.anthropic.claude-sonnet-5-5", "tools": [{"function": {"name": probe.TOOL}}]})
            entry["url"] = "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1/chat/completions"
            evidence = probe.analyze([entry], 1, "HTTP 404 Not Found", client + ":us.anthropic.claude-sonnet-5-5:thinking")
            self.assertFalse(evidence["accepted"])
            self.assertIn("client is not type: bedrock (Converse)", probe.verdict(evidence))
            self.assertIn("/openai/v1/chat/completions", probe.verdict(evidence))
            self.assertEqual(evidence["status"], "404")

    def test_custom_bedrock_prefix_and_trailing_converse_usage(self):
        model = "us.anthropic.claude-sonnet-5-5"
        initial = request({"toolConfig": {"tools": [{"toolSpec": {"name": probe.TOOL}}]}})
        initial["url"] = f"https://bedrock-runtime.us-east-1.amazonaws.com/model/{model}/converse-stream"
        replay = request({"messages": [{"content": [{"toolResult": {"content": []}}]}]}, second=2)
        replay["url"] = initial["url"]
        stop = lambda second: stream({"smithy_type": "messageStop", "event": {"stopReason": "end_turn"}}, "bedrock", second=second)
        metadata = stream({"smithy_type": "metadata", "event": {"usage": {"inputTokens": 20}}}, "bedrock", second=5)
        evidence = probe.analyze([initial, stop(1), replay, stop(4), metadata], 0, "", "bedrock-converse:" + model + ":thinking")
        self.assertTrue(evidence["accepted"])
        self.assertEqual(probe.usage_objects(evidence["replay"][0]["events"]), [{"inputTokens": 20}])
        invocation = probe.Invocation("probe", "session", "bedrock-converse:" + model + ":thinking", 600)
        cmd = probe.command(Path("harnx"), invocation)
        self.assertIn("bedrock-converse:" + model + ":thinking", cmd)

    def test_summary_settings(self):
        cases = [
            ({"reasoning": {"summary": "auto"}}, 'reasoning.summary="auto"'),
            ({"additionalModelRequestFields": {"thinking": {"display": "summarized"}}},
             'thinking.display="summarized"'),
            ({"generationConfig": {"thinkingConfig": {"includeThoughts": True}}},
             "includeThoughts=true"),
        ]
        for body, expected in cases:
            self.assertEqual(probe.summary_setting(body), expected)
        self.assertIn("absent", probe.summary_setting({}))

    def test_nonempty_streamed_reasoning_only(self):
        self.assertEqual(probe.thought_chars(stream({"type": "response.reasoning_summary_text.delta",
                                                    "delta": " thought "})), 7)
        self.assertEqual(probe.thought_chars(stream({"type": "response.reasoning_text.delta",
                                                    "delta": "thought"})), 7)
        self.assertEqual(probe.thought_chars(stream({"candidates": [{"content": {"parts": [
            {"thought": True, "text": "thought"}, {"text": "answer"}, {"thoughtSignature": "sig"}
        ]}}]}, "vertexai")), 7)
        self.assertEqual(probe.thought_chars(stream({"smithy_type": "contentBlockDelta",
            "event": {"delta": {"reasoningContent": {"text": "thought", "signature": "sig"}}}}, "bedrock")), 7)
        self.assertEqual(probe.thought_chars(stream({"smithy_type": "contentBlockDelta",
            "event": {"delta": {"reasoningContent": {"signature": "sig"}}}}, "bedrock")), 0)
        self.assertEqual(probe.thought_chars({"kind": "request", "body": {"thought": "thought"}}), 0)
        self.assertEqual(probe.thought_chars({"kind": "response", "body": {"thought": "thought"}}), 0)

    def test_null_terminal_fields(self):
        for body in ({"candidates": None}, {"candidates": [{"content": None}]},
                     {"candidates": [{"content": {"parts": None}}]}):
            self.assertEqual(probe.thought_chars(stream(body, "vertexai")), 0)
        self.assertFalse(probe.completed(stream({"candidates": None}, "vertexai")))
        self.assertTrue(probe.completed(stream({"candidates": [{"content": None, "finishReason": "STOP"}]}, "vertexai")))
        self.assertEqual(probe.thought_chars(stream({"smithy_type": "contentBlockDelta",
            "event": {"delta": {"reasoningContent": None}}}, "bedrock")), 0)
        self.assertTrue(probe.completed(stream({"smithy_type": "messageStop",
            "event": {"stopReason": "end_turn"}}, "bedrock")))

    def test_replay_shapes(self):
        for body in (
            {"input": [{"type": "function_call_output"}]},
            {"contents": [{"parts": [{"functionResponse": {}}]}]},
            {"messages": [{"content": [{"toolResult": {}}]}]},
            {"messages": [{"role": "tool", "content": "{}"}]},
        ):
            self.assertTrue(probe.has_tool_result(body))
        self.assertFalse(probe.has_tool_result({"input": [{"type": "function_call"}]}))

    def test_acceptance_requires_replay_completion_and_exit_zero(self):
        entries = openai_exchange()
        evidence = probe.analyze(entries, 0, "")
        self.assertTrue(evidence["accepted"])
        self.assertEqual(evidence["thoughts"], len("Check session"))
        self.assertEqual(evidence["status"], "status unavailable")
        self.assertFalse(probe.analyze(entries, 1, "HTTP status: 400")["accepted"])
        self.assertEqual(probe.analyze(entries, 1, "HTTP status: 400")["status"], "400")
        self.assert_replay_rejected(entries[:-1])
        self.assert_replay_rejected(entries[:3])
        self.assert_replay_rejected([])
        retried = entries[:-1] + [entries[-2], entries[-1]]
        self.assert_replay_rejected(retried)

    def test_usage_and_wall_time(self):
        usage = probe.usage_objects(openai_exchange())
        self.assertEqual(usage[-1]["output_tokens_details"]["reasoning_tokens"], 1)
        self.assertEqual(probe.seconds("2026-01-01T00:00:02.000Z", "2026-01-01T00:00:04.500Z"), "2.500s")
        self.assertEqual(probe.seconds(None, None), "unavailable")

    def test_redacts_auth_from_config_urls_and_environment(self):
        redactor = probe.Redactor({"AWS_SECRET_ACCESS_KEY": "environment-secret"})
        value = redactor.obj({
            "url": "https://user:pass@api.example.test?key=config-secret&x-amz-signature=sig&alt=sse",
            "Authorization": "Bearer hidden", "x-api-key": "key-from-config",
            "note": "environment-secret", "reasoning": {"summary": "auto"},
            "thoughtSignature": "opaque-replay-signature",
        })
        result = json.dumps(value)
        for secret in ("user:pass", "config-secret", "=sig", "hidden", "key-from-config", "environment-secret"):
            self.assertNotIn(secret, result)
        self.assertIn("opaque-replay-signature", result)
        self.assertIn("alt=sse", result)
        self.assertNotIn("config-secret", redactor.text("HTTP error https://api.example.test?key=config-secret"))

    def test_config_file_parent_is_agent_directory(self):
        self.assertEqual(probe.config_dir({"HARNX_CONFIG_FILE": "/tmp/probe-config/config.yaml"}), Path("/tmp/probe-config"))
        self.assertEqual(probe.config_dir({"HARNX_CONFIG_DIR": "/tmp/probe-config"}), Path("/tmp/probe-config"))

    def test_defaults_exist_in_catalog(self):
        provider, entries = None, set()
        for line in (probe.ROOT / "crates/harnx/models.yaml").read_text().splitlines():
            if line.startswith("- provider: "):
                provider = line.split(": ", 1)[1]
            elif line.startswith("    - name: "):
                entries.add((provider, line.split(": ", 1)[1]))
        for model in probe.DEFAULT_MODELS:
            client, name = model.split(":", 1)
            self.assertIn(("openai" if client == "codex" else client, name), entries)

    def test_dry_run_does_not_source_or_execute(self):
        with tempfile.TemporaryDirectory() as directory:
            args = argparse.Namespace(dry_run=True, models=[], output_dir=Path(directory), timeout_secs=600)
            stdout = io.StringIO()
            with mock.patch.object(probe, "credential_env", side_effect=AssertionError("credential access")), \
                 mock.patch.object(probe.subprocess, "run", side_effect=AssertionError("execution")), \
                 contextlib.redirect_stdout(stdout):
                self.assertEqual(probe.run_probe(args), 0)
            self.assertEqual(list(Path(directory).iterdir()), [])
            self.assertIn("HARNX_WORKER_BIN=", stdout.getvalue())
            self.assertIn("--agent reasoning-probe-", stdout.getvalue())
            self.assertNotIn("@local", stdout.getvalue())
            for model in probe.DEFAULT_MODELS:
                self.assertIn(model, stdout.getvalue())

    def test_analysis_exception_leaves_trace_private_and_redacted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output_dir = root / "reports"
            env = {"HARNX_CONFIG_DIR": str(root / "config"), "API_KEY": "fixture-secret"}
            bins = {"harnx": root / "harnx", "harnx-worker": root / "harnx-worker"}
            entries = openai_exchange()
            entries[0]["url"] = "https://[invalid?key=unknownsecret"
            entries[0]["headers"] = {"Authorization": "Bearer header-secret"}
            entries[0]["body"]["note"] = "fixture-secret"
            secrets = ("fixture-secret", "unknownsecret", "header-secret")

            def execute(cmd, **kwargs):
                trace = Path(kwargs["env"]["HARNX_LLM_TRACE"])
                write_trace(trace, entries, "\ninvalid trace fixture-secret\n")
                trace.chmod(0o644)
                return subprocess.CompletedProcess(cmd, 0, "fixture-secret", "Bearer header-secret")

            def fail_analysis(parsed, returncode, output, model):
                # Disk sanitization must not change the evidence passed to analysis.
                self.assertEqual(parsed, entries)
                self.assertEqual(returncode, 0)
                self.assertEqual(model, probe.DEFAULT_MODELS[0])
                for secret in secrets:
                    self.assertNotIn(secret, output)
                raise RuntimeError("Synthetic analysis failure")

            previous_umask = os.umask(0o077)
            self.addCleanup(os.umask, previous_umask)
            with mock.patch.object(probe, "credential_env", return_value=env), \
                 mock.patch.object(probe, "build_binaries", return_value=bins), \
                 mock.patch.object(probe.subprocess, "run", side_effect=execute), \
                 mock.patch.object(probe, "analyze", side_effect=fail_analysis) as analyze, \
                 mock.patch.object(probe.sys, "argv", ["probe", probe.DEFAULT_MODELS[0],
                                                      "--output-dir", str(output_dir)]), \
                 contextlib.redirect_stdout(io.StringIO()) as stdout, \
                 contextlib.redirect_stderr(io.StringIO()) as stderr:
                self.assertEqual(probe.main(), 1)
            analyze.assert_called_once()
            trace = next(output_dir.rglob("*.trace.jsonl"))
            retained = [json.loads(line) for line in trace.read_text().splitlines()]
            self.assertEqual(retained, [probe.Redactor(env).obj(entry) for entry in entries])
            self.assertEqual(trace.stat().st_mode & 0o777, 0o600)
            self.assertEqual(list((root / "config/agents").iterdir()), [])
            self.assertIn("Probe setup failed: Synthetic analysis failure", stderr.getvalue())
            for secret in secrets:
                self.assertNotIn(secret, stdout.getvalue())
                self.assertNotIn(secret, stderr.getvalue())
            self.assert_artifacts_redacted(output_dir, secrets)

    def test_absent_trace_retains_empty_private_artifact_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = {"HARNX_CONFIG_DIR": str(root / "config")}
            bins = {"harnx": root / "harnx", "harnx-worker": root / "harnx-worker"}
            args = argparse.Namespace(dry_run=False, models=probe.DEFAULT_MODELS[:1],
                                      output_dir=root / "reports", timeout_secs=600)
            result = subprocess.CompletedProcess([], 0, "", "")
            previous_umask = os.umask(0o077)
            self.addCleanup(os.umask, previous_umask)
            with mock.patch.object(probe, "credential_env", return_value=env), \
                 mock.patch.object(probe, "build_binaries", return_value=bins), \
                 mock.patch.object(probe.subprocess, "run", return_value=result), \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(probe.run_probe(args), 1)
            trace = next(args.output_dir.rglob("*.trace.jsonl"))
            self.assertEqual(trace.read_text(), "")
            self.assertEqual(trace.stat().st_mode & 0o777, 0o600)
            report = next(args.output_dir.glob("*.md")).read_text()
            self.assertIn("FAILED / not demonstrated; status unavailable; exit 0", report)
            self.assertIn("request unavailable", report)
            self.assertIn("invalid trace lines: 0", report)
            self.assertEqual(list((root / "config/agents").iterdir()), [])

    def test_offline_full_run_writes_private_redacted_report_and_fails_any_model(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_paths = {"harnx": root / "target/harnx", "harnx-worker": root / "target/harnx-worker"}
            env = {"HARNX_CONFIG_DIR": str(root / "config"), "API_KEY": "fixture-secret",
                   "HARNX_NATS_SERVER": "remote"}
            args = argparse.Namespace(dry_run=False, models=probe.DEFAULT_MODELS[:2],
                                      output_dir=root / "reports", timeout_secs=600)
            calls = []
            def execute(cmd, **kwargs):
                calls.append(cmd)
                if cmd == probe.BUILD:
                    data = cargo_output(bin_paths)
                    return subprocess.CompletedProcess(cmd, 0, data, "built")
                self.assertEqual(kwargs["env"]["HARNX_WORKER_BIN"], str(bin_paths["harnx-worker"]))
                self.assertEqual(kwargs["env"]["HARNX_NATS_SERVER"], "")
                self.assertNotIn("@", cmd[cmd.index("--agent") + 1])
                successful = cmd[cmd.index("--model") + 1].startswith("openai:")
                entries = openai_exchange() if successful else openai_exchange()[:-1]
                write_trace(Path(kwargs["env"]["HARNX_LLM_TRACE"]), entries)
                return subprocess.CompletedProcess(cmd, 0 if successful else 1, "fixture-secret", "" if successful else "HTTP 400 Bad Request")
            previous_umask = os.umask(0o077)
            self.addCleanup(os.umask, previous_umask)
            with mock.patch.object(probe, "credential_env", return_value=env), \
                 mock.patch.object(probe.subprocess, "run", side_effect=execute), \
                 contextlib.redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(probe.run_probe(args), 1)
            self.assertNotIn("fixture-secret", stdout.getvalue())
            report = next(args.output_dir.glob("*.md"))
            text = report.read_text()
            self.assertIn("accepted; status unavailable; exit 0", text)
            self.assertIn("FAILED / not demonstrated; 400; exit 1", text)
            self.assertIn("reasoning_tokens", text)
            self.assertIn("2.000s", text)
            self.assertEqual(report.stat().st_mode & 0o777, 0o600)
            self.assertEqual(list((root / "config/agents").iterdir()), [])
            self.assert_artifacts_redacted(args.output_dir, ("fixture-secret",))
            self.assertEqual(len(calls), 3)

    def test_malformed_wire_shapes_do_not_count_as_reasoning_or_completion(self):
        cases = [
            stream(None), stream("invalid"),
            stream({"type": "response.reasoning_summary_text.delta", "delta": 12}),
            stream({"candidates": [None, "invalid", {"content": {"parts": [None, {"thought": True, "text": 9}]}}]}, "vertexai"),
            stream({"candidates": {"finishReason": "STOP"}}, "gemini"),
            stream({"smithy_type": "contentBlockDelta", "event": {"delta": "invalid"}}, "bedrock"),
            stream({"smithy_type": "messageStop", "event": None}, "bedrock"),
        ]
        for entry in cases:
            with self.subTest(entry=entry):
                self.assertEqual(probe.thought_chars(entry), 0)
                self.assertFalse(probe.completed(entry))
                self.assert_replay_rejected([request(None), entry])
        self.assert_replay_rejected([None, [], "invalid"])

    def test_openai_terminal_payloads_fail_closed_for_null_or_malformed_response(self):
        for response in (None, [], "invalid", {"status": None}, {"status": "failed"}, {"status": "incomplete"}):
            entries = openai_exchange()
            entries[-1] = stream({"type": "response.completed", "response": response})
            self.assert_replay_rejected(entries)
        self.assertFalse(probe.completed(stream({"type": "response.completed"})))
        self.assertTrue(probe.completed(stream({"type": "response.completed", "response": {}})))

    def test_summary_setting_precedence_and_malformed_containers(self):
        body = {"reasoning": {"summary": None},
                "additionalModelRequestFields": {"thinking": {"display": "summarized"}},
                "generationConfig": {"thinkingConfig": {"includeThoughts": False}}}
        self.assertEqual(probe.summary_setting(body), "reasoning.summary=null")
        body["reasoning"] = "invalid"
        self.assertEqual(probe.summary_setting(body), 'thinking.display="summarized"')
        body["additionalModelRequestFields"] = None
        self.assertEqual(probe.summary_setting(body), "includeThoughts=false")
        for body in (None, [], {"reasoning": None}, {"generationConfig": {"thinkingConfig": "invalid"}}):
            self.assertIn("absent", probe.summary_setting(body))

    def test_bound_response_id_separates_same_model_title_and_late_prior_event(self):
        entries = openai_exchange()
        entries[1] = stream({"type": "response.created", "response": {"id": "first"}})
        entries[2]["body"]["response"]["id"] = "first"
        entries[-1]["body"]["response"]["id"] = "second"
        bound = stream({"type": "response.created", "response": {"id": "second"}}, second=3)
        late = stream({"type": "response.completed", "response": {"id": "first", "status": "completed"}}, second=3)
        title = request({"input": "title"}, second=3)
        evidence = probe.analyze(entries[:-1] + [bound, title, late, entries[-1]], 0, "")
        self.assertTrue(evidence["accepted"])
        self.assertEqual(len(evidence["attempts"][0]["events"]), 3)
        self.assertEqual(len(evidence["replay"][0]["events"]), 2)
        self.assertEqual(evidence["auxiliary"][0]["events"], [])
        self.assert_replay_rejected(entries[:-1] + [bound, title, late])

    def test_trace_parser_counts_invalid_json_and_nonobject_entries(self):
        lines = [json.dumps(entry) for entry in openai_exchange()]
        lines.extend(["invalid fixture-secret", "null", "[]", '"fixture-secret"', ""])
        entries, invalid = probe.parse_trace_lines(lines)
        self.assertEqual(entries, openai_exchange())
        self.assertEqual(invalid, 5)

    def test_offline_trace_rejects_corruption_without_writing(self):
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / "01.trace.jsonl"
            trace.write_text(json.dumps(request({})) + "\nnull\n")
            before = trace.read_bytes(), trace.stat().st_mtime_ns
            with self.assertRaisesRegex(RuntimeError, "refusing incomplete offline evidence"):
                probe.offline_trace(trace)
            self.assertEqual((trace.read_bytes(), trace.stat().st_mtime_ns), before)
            trace.write_text("\n" + json.dumps(request({})) + "\n\n")
            self.assertEqual(probe.offline_trace(trace), [request({})])

    def test_cargo_artifact_parsing_and_override_validation(self):
        bins = {name: (probe.ROOT / "target/debug" / name).resolve() for name in ("harnx", "harnx-worker")}
        self.assertEqual(probe.cargo_binaries(cargo_output(bins)), bins)
        probe.validate_binaries(bins, {"HARNX_BIN": str(bins["harnx"]), "HARNX_WORKER_BIN": str(bins["harnx-worker"])})
        with self.assertRaisesRegex(RuntimeError, "did not produce a native harnx-worker"):
            probe.validate_binaries({"harnx": bins["harnx"]}, {})
        with self.assertRaisesRegex(RuntimeError, "HARNX_WORKER_BIN does not match freshly built artifact"):
            probe.validate_binaries(bins, {"HARNX_WORKER_BIN": "/installed/harnx-worker"})

    def test_timeout_output_handles_bytes_text_and_absent_streams(self):
        for stdout, stderr, expected in ((b"partial\xff", b"failure", "partial\ufffd"),
                                         ("partial", "failure", "partial"), (None, None, "")):
            error = subprocess.TimeoutExpired(["harnx"], 77, output=stdout, stderr=stderr)
            with mock.patch.object(probe.subprocess, "run", side_effect=error) as execute:
                result, elapsed = probe.run_command(["harnx"], {}, 17)
            self.assertEqual(result.returncode, 124)
            self.assertEqual(result.stdout, expected)
            self.assertIn("Probe observer timeout: CLI did not exit within allowance + 60s", result.stderr)
            self.assertGreaterEqual(elapsed, 0)
            self.assertEqual(execute.call_args.kwargs["timeout"], 77)

    def test_custom_dry_run_uses_target_directory_and_timeout_without_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = argparse.Namespace(dry_run=True, models=["aws-prod:us.anthropic.claude-sonnet-5-5:thinking"],
                                      output_dir=root, timeout_secs=17)
            with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": str(root / "custom target")}), \
                 mock.patch.object(probe, "credential_env", side_effect=AssertionError("credentials")), \
                 mock.patch.object(probe.subprocess, "run", side_effect=AssertionError("execution")), \
                 contextlib.redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(probe.run_probe(args), 0)
            output = stdout.getvalue()
            self.assertIn(str(root / "custom target/debug/harnx-worker"), output)
            self.assertIn("--model aws-prod:us.anthropic.claude-sonnet-5-5:thinking --timeout-secs 17", output)
            self.assertEqual(list(root.iterdir()), [])

    def test_worker_isolation_keeps_credentials_and_removes_targeting(self):
        targeting = ("HARNX_SESSION_ID", "HARNX_AGENT_NAME", "HARNX_CLUSTER", "HARNX_LLM_TRACE")
        env = dict.fromkeys(targeting, "ambient")
        env.update({"API_KEY": "fixture-secret", "HARNX_NATS_SERVER": "remote"})
        probe.isolate_worker(env, {"harnx-worker": Path("/built/harnx-worker")})
        self.assertEqual(env, {"API_KEY": "fixture-secret", "HARNX_NATS_SERVER": "",
                               "HARNX_WORKER_BIN": "/built/harnx-worker"})

    def test_setup_failure_does_not_delete_colliding_agent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = argparse.Namespace(models=probe.DEFAULT_MODELS[:1], output_dir=root / "reports", timeout_secs=600)
            runner = probe.ProbeRunner(args)
            env = {"HARNX_CONFIG_DIR": str(root / "config")}
            bins = {"harnx": root / "harnx", "harnx-worker": root / "harnx-worker"}
            agent_file = root / "config/agents" / f"{runner.agent}.md"
            agent_file.parent.mkdir(parents=True)
            agent_file.write_text("pre-existing agent")
            previous_umask = os.umask(0o077)
            self.addCleanup(os.umask, previous_umask)
            with mock.patch.object(probe, "credential_env", return_value=env), \
                 mock.patch.object(probe, "build_binaries", return_value=bins), \
                 mock.patch.object(probe.subprocess, "run", side_effect=AssertionError("execution")), \
                 self.assertRaises(FileExistsError):
                runner.run()
            self.assertEqual(agent_file.read_text(), "pre-existing agent")

    def test_invalid_trace_line_fails_run_even_when_replay_is_completed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = {"HARNX_CONFIG_DIR": str(root / "config"), "API_KEY": "fixture-secret"}
            bins = {"harnx": root / "harnx", "harnx-worker": root / "harnx-worker"}
            args = argparse.Namespace(dry_run=False, models=probe.DEFAULT_MODELS[:1], output_dir=root / "reports", timeout_secs=600)

            def execute(cmd, **kwargs):
                write_trace(Path(kwargs["env"]["HARNX_LLM_TRACE"]), openai_exchange(), "\ninvalid fixture-secret\n")
                return subprocess.CompletedProcess(cmd, 0, "", "")

            previous_umask = os.umask(0o077)
            self.addCleanup(os.umask, previous_umask)
            with mock.patch.object(probe, "credential_env", return_value=env), \
                 mock.patch.object(probe, "build_binaries", return_value=bins), \
                 mock.patch.object(probe.subprocess, "run", side_effect=execute), \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(probe.run_probe(args), 1)
            report = next(args.output_dir.glob("*.md")).read_text()
            self.assertIn("accepted; status unavailable; exit 0", report)
            self.assertIn("invalid trace lines: 1", report)
            self.assert_artifacts_redacted(args.output_dir, ("fixture-secret",))
            self.assertEqual(list((root / "config/agents").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
