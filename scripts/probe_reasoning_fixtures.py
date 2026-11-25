"""Synthetic provider exchanges and JSONL artifacts for offline probe tests."""

import json

if __package__:
    from . import probe_reasoning_summaries as probe
else:
    import probe_reasoning_summaries as probe


def request(body, second=0):
    return {"kind": "request", "ts": f"2026-01-01T00:00:{second:02d}.000Z",
            "url": "https://api.example.test/v1/responses?key=fixture-secret", "body": body}


def stream(body, provider="openai_responses", second=1):
    return {"kind": "stream-event", "provider": provider,
            "ts": f"2026-01-01T00:00:{second:02d}.000Z", "body": body}


def openai_exchange():
    return [
        request({"tools": [{"name": probe.TOOL}], "reasoning": {"summary": "auto"}}),
        stream({"type": "response.reasoning_summary_text.delta", "delta": "Check session"}),
        stream({"type": "response.completed", "response": {"status": "completed"}}),
        request({"input": [{"type": "function_call_output", "output": "{}"}],
                 "reasoning": {"summary": "auto"}}, second=2),
        stream({"type": "response.completed", "response": {"status": "completed",
                "usage": {"input_tokens": 10, "output_tokens": 2,
                          "output_tokens_details": {"reasoning_tokens": 1}}}}, second=4),
    ]


def claude_turn(stop_reason, thinking, second):
    """One Messages stream, in the shapes of a traced Fable 5.2 turn."""
    events = [
        {"type": "message_start", "message": {"model": "claude-fable-5-2", "id": "msg_fixture",
            "usage": {"input_tokens": 4}}},
        {"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}},
        {"type": "ping"},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": thinking}},
        {"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}},
        {"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "answer"}},
        {"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 9}},
        {"type": "message_stop"},
    ]
    return [stream(event, "claude", second=second) for event in events]


def claude_exchange():
    tools = [{"name": probe.TOOL, "input_schema": {}}]
    thinking = {"type": "adaptive", "display": "summarized"}
    initial = request({"model": "claude-fable-5-2", "tools": tools, "thinking": thinking,
        "messages": [{"role": "user", "content": "probe"}]})
    replay = request({"model": "claude-fable-5-2", "tools": tools, "thinking": thinking, "messages": [
        {"role": "user", "content": "probe"},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "plan"},
            {"type": "tool_use", "id": "toolu_fixture", "name": probe.TOOL, "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_fixture", "content": "{}"}]},
    ]}, second=2)
    for entry in (initial, replay):
        entry["url"] = "https://api.anthropic.com/v1/messages"
    return [initial, *claude_turn("tool_use", "Check session", 1), replay, *claude_turn("end_turn", "continuing", 4)]


def cargo_output(bins):
    records = [None, [], {"reason": "build-finished"},
               {"reason": "compiler-artifact", "target": {"name": {}}, "executable": "/invalid"},
               {"reason": "compiler-artifact", "target": None, "executable": "/invalid"}]
    records.extend({"reason": "compiler-artifact", "target": {"name": name}, "executable": str(path)}
                   for name, path in bins.items())
    return "\n".join(["compiler progress", *(json.dumps(item) for item in records)])


def openai_title_exchange(client, title, title_event):
    entries = openai_exchange()
    endpoint = "https://api.openai.com/v1/responses" if client == "openai" else "https://chatgpt.com/backend-api/codex/responses"
    for entry in entries:
        if entry["kind"] == "request":
            entry["url"] = endpoint
            entry["body"]["model"] = "gpt-5.6-sol"
        elif entry["body"].get("type") == "response.completed":
            entry["body"]["response"]["model"] = "gpt-5.6-sol"
    if client == "openai":
        entries[1] = stream({"type": "response.output_item.done", "item": {"type": "reasoning", "summary": []}})
    created = stream({"type": "response.created", "response": {
        "id": "probe-turn-two", "model": "gpt-5.6-sol", "status": "in_progress"}}, second=3)
    delta = stream({"type": "response.output_text.delta", "delta": "confirmed"}, second=3)
    return entries[:-1] + [title, title_event, created, delta, entries[-1]]


def concurrent_title_exchange(client):
    """Minimal shapes from user traces 01/02/03; no credentials or signatures."""
    title_model = "gemini-3.1-flash-lite"
    title = request({"contents": [], "generationConfig": {}, "systemInstruction": {}})
    title["url"] = f"https://generativelanguage.googleapis.com/v1beta/models/{title_model}:generateContent"
    title_event = stream({"modelVersion": title_model, "responseId": "title-response",
        "candidates": [{"content": {"parts": [{"text": "title"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 999}}, "vertexai", second=3)
    if client in ("openai", "codex"):
        return openai_title_exchange(client, title, title_event)
    model = "gemini-3.8-flash"
    initial = request({"tools": [{"functionDeclarations": [{"name": probe.TOOL}]}],
        "contents": [], "generationConfig": {"thinkingConfig": {"includeThoughts": True}}})
    initial["url"] = f"https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
    first = stream({"modelVersion": model, "responseId": "probe-turn-one",
        "candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"thought": True, "text": "reasoning"}, {"text": "call metadata"},
            {"functionCall": {"name": probe.TOOL, "args": {}}}]}}],
        "usageMetadata": {"promptTokenCount": 286, "thoughtsTokenCount": 200}}, "vertexai")
    replay = request({"tools": initial["body"]["tools"], "contents": [{"parts": [
        {"functionResponse": {"name": probe.TOOL, "response": {}}}]}]}, second=2)
    replay["url"] = initial["url"]
    final = stream({"modelVersion": model, "responseId": "probe-turn-two",
        "candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"thought": True, "text": "continuing"}, {"text": "confirmed"}]}}],
        "usageMetadata": {"promptTokenCount": 1019, "thoughtsTokenCount": 551}}, "vertexai", second=4)
    return [initial, first, replay, title, title_event, final]


def numbered_exchange(index):
    entries = openai_exchange()
    entries[1]["body"]["delta"] = "x" * index
    for entry in entries:
        if entry["kind"] == "request":
            entry["body"]["model"] = f"model-{index}"
    return entries


def write_trace(trace, entries, suffix=""):
    trace.write_text("\n".join(json.dumps(entry) for entry in entries) + suffix)
