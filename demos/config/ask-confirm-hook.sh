#!/usr/bin/env bash
# Hook script that requires manual confirmation for every tool call.
# Used by the tool-confirm demo and as a reference for docs.
#
# Input: JSON payload on stdin (from harnx PreToolUse event)
# Output: JSON response requesting user confirmation

# Read and discard stdin (required by protocol)
cat > /dev/null

# Return "ask" permission decision
printf '%s\n' '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Manual approval required"}}'
