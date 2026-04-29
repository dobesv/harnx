---
harnx: patch
---
fix(acp): propagate `session/cancel` to nested ACP sub-agents on Ctrl-C / cancel.

Two bugs sat in series:

1. `<AcpManager as ToolProvider>::call_tool` raced the inner call against `wait_abort_signal(abort)` in an outer `tokio::select!` and on abort just dropped the inner future. The actual `session_cancel` dispatch lived inside that future (gated on `tokio::signal::ctrl_c()`, which never fires while the TUI is in raw mode), so it never ran. Late chunks from the still-running sub-agent then leaked through `AcpNotificationClient`'s no-forwarder fallback into the parent transcript.

2. `HarnxAgent::prompt` raced `run_agent_loop` against `cancel_notify` in another `tokio::select!`. When cancel arrived (the outermost case in pure-ACP-server mode), it synchronously dropped `run_agent_loop` — and any `AcpManager::call_tool` inside it — before the `AcpManager`'s abort handler could observe `abort_signal` and dispatch `session/cancel` further down. The sub-agent process kept running.

The fix plumbs the `AbortSignal` into `AcpManager::call_tool_inner`'s `session_prompt` branch so abort drives the existing cancel path, and replaces the hard-cancel `tokio::select!` in `HarnxAgent::prompt` with a two-stage cancel: cooperatively wait for `run_agent_loop` to unwind on `abort_signal` (giving nested ACP layers time to dispatch their own cancels), with a 100 ms grace before falling back to a `select!`-style hard-cancel for layers that don't observe abort.
