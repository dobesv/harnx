"""Claude-protocol fixture. Used by crate and middleware integration tests."""
import json
import os
import sys
import time
from pathlib import Path

mode = sys.argv[1]
count = 0


def response(payload):
    global count
    count += 1
    action = payload.get("tool_input", {}).get("action", mode)
    marker = payload.get("marker")
    if marker:
        Path(marker).write_text(str(count))
    if action == "hang":
        time.sleep(60)
    if action == "oversize":
        return {"additionalContext": "x" * (256 * 1024)}
    if action == "stderr":
        sys.stderr.write("x" * (512 * 1024))
        sys.stderr.flush()
    if action == "exit2":
        sys.stderr.write("blocked by exit 2")
        sys.exit(2)
    if action == "exit1":
        sys.stderr.write("fixture failure")
        sys.exit(1)
    if action == "text":
        print("plain context", flush=True)
        sys.exit(0)
    if action == "broken":
        print("{broken", flush=True)
        sys.exit(0)
    if action == "deny":
        return {"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "blocked by policy"}}
    if (action == "ask-pre" and payload["hook_event_name"] == "PreToolUse") or (action == "ask-post" and payload["hook_event_name"] == "PostToolUse"):
        return {"hookSpecificOutput": {"permissionDecision": "ask", "permissionDecisionReason": "Review this operation?"}}
    if payload["hook_event_name"] == "PreToolUse":
        arguments = dict(payload.get("tool_input", {}))
        arguments["command"] = "rewritten"
        return {"hookSpecificOutput": {"updatedInput": arguments}, "additionalContext": "pre context",
                "diagnostics": {"count": count, "id": payload.get("id"), "cwd": os.getcwd(),
                                "package_dir": os.environ.get("HARNX_PACKAGE_DIR"),
                                "protocol": os.environ.get("HARNX_HOOK_PROTOCOL"),
                                "extra": payload.get("extra")}}
    return {"hookSpecificOutput": {"toolResponse": {"post": payload.get("tool_response")}, "additionalContext": "post context"}}


if mode == "--persistent":
    assert os.environ["HARNX_HOOK_PROTOCOL"] == "jsonl"
    if "--eager-notice" in sys.argv:
        print(json.dumps({"notice": {"message": "x" * (128 * 1024)}}), flush=True)
    for line in sys.stdin:
        payload = json.loads(line)
        result = response(payload)
        if payload.get("tool_input", {}).get("action") == "notice":
            print(json.dumps({"notice": {"message": "working"}}), flush=True)
        result["id"] = "wrong" if payload.get("tool_input", {}).get("action") == "wrong_id" else payload["id"]
        print(json.dumps(result), flush=True)
else:
    print(json.dumps(response(json.load(sys.stdin))), flush=True)
