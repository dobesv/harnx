---
title: "Web handoff CUSTOM event double-dispatch"
date: 2026-09-10
last_verified: 2026-09-10
component: "web"
problem_type: integration_issue
status: current
anchors:
  - web/src/ChatProvider.tsx:153-159
  - web/src/__tests__/ChatProvider.test.ts:227-243
tags:
  - ag-ui
  - custom-events
  - handoff
plan_ref: "web-handoff-attach-seq"
---

# Web handoff CUSTOM event double-dispatch

## When this is relevant
Implementing AG-UI CUSTOM event handlers in the Web UI, or debugging why a CUSTOM event appears to be processed twice.

## Durable lesson
`@ag-ui/client`'s HTTP agent routes every CUSTOM frame through **both** `subscriber.onEvent` and `subscriber.onCustomEvent`. Handling the event in both hooks double-processes it.

Fix: route custom-event handling through exactly one hook. In `HarnxHttpAgent`, the `onEvent` hook calls `handleAgentEvent` which dispatches CUSTOM events to `handleCustomEvent`. The `onCustomEvent` hook forwards to the downstream subscriber without re-processing.

## Evidence and current anchors
- `web/src/ChatProvider.tsx:153-159` — `wrapSubscriber` shows both hooks; `onEvent` handles CUSTOM, `onCustomEvent` only forwards
- `web/src/__tests__/ChatProvider.test.ts:227-243` — test verifies single dispatch when `onEvent` fires then `onCustomEvent` fires

## Failed approaches or trade-offs
None discovered. The fix was straightforward once the double-dispatch was identified.
