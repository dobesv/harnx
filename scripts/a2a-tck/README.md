# Run A2A conformance diagnostics

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
response. Script returns the TCK exit status, including failures.

## Why CI is non-gating

Task 12's feasibility fallback applies. Initial unmodified MUST run produced
**56 passed, 16 failed, 163 skipped, 30 deselected**. The TCK's skips include
unselected transports, undeclared capabilities, and unmet scenario
preconditions; they don't prove those behaviors conform.

The spike ADR's feasibility verdict was based on inspection, not a real run.
The pinned TCK cannot serve as a MUST gate for this server profile:

- `tck/requirements/base.py::tck_id` returns the same ID for the same name
  throughout a run. `CORE-SEND-001`, `CORE-EXECUTION-MODE-001/002`, and
  `CORE-MULTI-001a/003` reuse `tck-complete-task` with different parts. Our
  required deduplication rejects changed payloads under the same messageId.
- `CORE-MULTI-002a` expects rejection of a client-generated contextId, but
  doesn't set `expected_error`. `test_requirements.py::_validate_response`
  therefore fails the correct TaskNotFound rejection before custom validators
  run. `CORE-SEND-003` has the same validator defect: any error fails the test.
- `CORE-LIST-001` through `CORE-LIST-005` use `tck-test-context`, a
  client-generated context never allocated by the server. Returning not-found
  is required by our ownership/context policy. These tests aren't missing a
  scoped ListTasks implementation; they use an inaccessible scope.
- `test_artifacts.py` sends identical text (`TCK artifact test`) and selects
  output by messageId alone. It requires text `Generated text content`, raw
  files, URL files, structured data, and a direct Message response. Our runner
  produces text artifacts in Tasks. messageId is protocol bookkeeping, not
  model input. No deterministic LLM can distinguish these requests or emit
  those other protocol output types through the approved text-artifact map.

No individual tests are waived or deselected beyond the upstream
`--transport jsonrpc --level must` selection. No patch is applied to the TCK.
The advisory workflow retains failures and uploads their reports rather than
claiming MUST conformance. Full crate nextest tests remain the gating coverage
for streaming, cancel, scoped list, deduplication, and server-owned contexts.

The server returns ContentTypeNotSupportedError (`-32005`) for unsupported
raw media types (per commit 33dcf42cd). The TCK test `CORE-SEND-003` still
fails because its validator lacks `expected_error` and unconditionally rejects
any error response.

Before making this job gating, use a reviewed TCK revision that fixes duplicate
IDs and expected-error validation and supports a text-Task-only profile with
server-allocated contexts. Re-run every MUST item and review unmet preconditions.
Don't add production messageId switches or weaken context ownership to satisfy
fixtures.
