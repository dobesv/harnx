# Run A2A conformance checks

Run from the repository root (the script also accepts invocation by absolute path):

```sh
PROTOC=/usr/bin/protoc scripts/run-a2a-tck.sh
```

Requires Rust (workspace toolchain), Python 3.11+, `uv`, `git`, and
`nats-server` on PATH. CI leaves `PROTOC` unset so `a2a-pb` uses its vendored
compiler. Local Cargo registries mounted `noexec` need a system compiler.

The script builds real `harnx-a2a-server` and `harnx-worker` binaries with the
lockfile unchanged. It fetches `a2aproject/a2a-tck` at
`263b9cfaf16a554bdfb166a7ba5b67716e946349`, verifies HEAD, and installs Python
dependencies with `uv sync --frozen`. Temporary config, data, state, and
JetStream directories isolate the run from existing sessions. Broker, mock
LLM, and A2A server use dynamically allocated loopback ports. A real turn
must complete before the TCK starts.

`harness.py` serves a test-only OpenAI-compatible streaming endpoint returning
`Hello world`. It records actual worker requests. Production code has no test
mode, messageId interception, or protocol response rewriting. Process groups
are terminated and reaped on normal completion, failure, or interruption.

Reports default to `target/a2a-tck-reports/`. Set `A2A_TCK_REPORT_DIR` to choose
another directory. Each invocation writes its own subdirectory so stale JUnit
files can't be mistaken for a new result. Reports include `junitreport.xml`,
TCK HTML/JSON reports, server/worker/broker logs, LLM requests, and the smoke
response. The script returns the TCK's pytest exit status.

## Gating CI with per-test waivers

The **A2A TCK / MUST conformance** job is gating. The unmodified pinned suite
produced **56 passed, 16 failed, 163 skipped, 30 deselected**. The 16 known
failures are listed individually in [`waivers.toml`](waivers.toml), with exact
pytest node IDs, categories, and reasons. With waivers, the expected pytest
result is **56 passed, 16 xfailed, 163 skipped, 30 deselected**, with no failures.
Skips include unselected transports, undeclared capabilities, and unmet scenario
preconditions; neither skips nor waived failures prove conformance.

The harness puts this directory on the TCK subprocess's `PYTHONPATH` and passes
`-- -p a2a_waivers -rx` to `run_tck.py`. `a2a_waivers.py` reads TOML with Python's
stdlib `tomllib` and adds `pytest.mark.xfail(strict=True, reason=...)` during
collection. Waived tests still execute; they aren't skipped or excluded.
Expected failures show `XFAIL` plus category/reason in console and pytest reports.
JUnit encodes xfails as skipped elements with `type="pytest.xfail"`, separate
from ordinary skips. The TCK's own compatibility JSON/HTML reports still record the underlying
conformance failures; they don't apply our waivers. Use pytest's summary and
JUnit report for the CI result.

Any unwaived failure fails the run. A waived test that passes produces
`XPASS(strict)` and also fails the run, so an obsolete waiver cannot silently
remain. A waiver matching no collected test raises a usage error naming its
node ID, which catches renamed or removed tests when updating the pin. Matching
is exact, including the class and `[jsonrpc]` parameter; no other transport or
requirement is waived. Collection validation runs before `-m`/`-k` deselection.

### Waiver categories

- **design-conflict (9):** Four requirements (`CORE-EXECUTION-MODE-001/002`,
  `CORE-MULTI-001a/003`) reuse the `complete-task` messageId from `CORE-SEND-001`
  with different parts. Required deduplication rejects changed payloads under an
  existing messageId. Five requirements (`CORE-LIST-001` through `CORE-LIST-005`)
  use the client-generated `tck-test-context`, not a server-returned contextId.
  Scoped ListTasks correctly rejects that inaccessible scope.
- **tck-defect (2):** `CORE-MULTI-002a` expects rejection of a client-generated
  contextId but omits `expected_error`, so the validator rejects the correct
  TaskNotFound response before custom checks. `CORE-SEND-003` has the same defect
  and rejects the server's correct ContentTypeNotSupportedError (`-32005`).
- **harness-limitation (5):** `test_artifacts.py` sends identical text (`TCK
  artifact test`) and selects output by messageId alone. It requires exact text
  `Generated text content`, raw files, URL files, structured data, or a direct
  Message response. The real runner produces text artifacts in Tasks; messageId
  isn't model input. A deterministic LLM cannot distinguish these prompts or
  emit those other protocol types through the approved output mapping.

No patch is applied to the TCK. Don't add production messageId switches or
weaken context ownership to satisfy fixtures. Full crate nextest tests also
cover streaming, cancel, scoped list, deduplication, and server-owned contexts.

### Remove a waiver

1. Fix the upstream test, harness limitation, or reviewed design conflict.
   Updating the TCK pin also requires checking every node ID and unmet precondition.
2. Run `scripts/run-a2a-tck.sh`. A newly passing waived test fails with
   `XPASS(strict)` and its reason.
3. Remove that test's entire `[[waiver]]` entry from `waivers.toml`. Keep unrelated
   entries unchanged. An empty waiver file is valid when none remain.
4. Rerun the script and inspect the fresh JUnit report. The test must now pass
   normally, with no unexpected failures or stale-waiver errors.

## Test the waiver plugin

From the repository root, in a Python 3.11+ environment with `pytest` installed
(the pinned TCK environment already includes it):

```sh
python3 -m pytest -q scripts/a2a-tck/test_waivers.py
```

These tests cover strict xfail, stale and malformed entries, exact node ID
matching, deselection, empty waiver files, and existing non-strict markers.
