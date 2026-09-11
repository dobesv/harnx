import { http, HttpResponse } from 'msw';
import {
  additionalSnapshot,
  createSessionEventsStream,
  finishExchange,
  isPromptlessRun,
  controlStates,
  addControlState,
  snapshots,
  notify,
  persistGalleryExchange,
  broadcastLiveEvent,
  persistSubAgentExchange,
  persistExchange,
  
} from './sessionUpdates';

const SUB_AGENT_SESSION_ID = 'child-session-0001';
const activeSessions = new Set<string>();
const openRunControllers = new Map<string, Set<any>>();
let subAgentExchangeId = 0;

function subAgentProgress(invocationId: string, status: 'running' | 'done', elapsedMs: number) {
  return {
    invocation_id: invocationId,
    agent: 'researcher',
    session_id: SUB_AGENT_SESSION_ID,
    status,
    elapsed_ms: elapsedMs,
    usage: { input_tokens: 1200, output_tokens: 345, cached_tokens: 67 },
    tool_call_count: 4,
  };
}

function isSseRequest(request: Request): boolean {
  return request.headers.get('accept')?.includes('text/event-stream') ?? false;
}

async function isRpcRequest(request: Request): Promise<boolean> {
  if (isSseRequest(request)) return false;
  if (request.headers.get('content-type')?.startsWith('application/json')) {
    return true;
  }
  try {
    const body = await request.clone().json() as { jsonrpc?: string };
    return body.jsonrpc === '2.0';
  } catch {
    return false;
  }
}

function encodeSseEvent(data: unknown): Uint8Array {
  return new TextEncoder().encode(`event: message\ndata: ${JSON.stringify(data)}\n\n`);
}

const SESSION_ONE_SNAPSHOT = [
  {
    id: 'm-system',
    role: 'system',
    content: 'You are mock system prompt content that should be collapsed by default.',
  },
  {
    id: 'm1',
    role: 'assistant',
    content: 'Hello from mock session',
  },
];

const HANDOFF_TARGET_SNAPSHOT = [
  {
    id: 'handoff-user',
    role: 'user',
    content: 'Delegated work from coding/coder',
  },
  {
    id: 'handoff-assistant',
    role: 'assistant',
    content: 'Durable handoff target history',
  },
];

function buildSnapshot(session: string) {
  if (session === 'handoff-target') return HANDOFF_TARGET_SNAPSHOT;
  if (session === SUB_AGENT_SESSION_ID) {
    return [
      { id: 'child-user', role: 'user', content: 'Research this task' },
      { id: 'child-assistant', role: 'assistant', content: 'Child task complete.' },
    ];
  }
  if (session === 'session-gallery') {
    return [
      {
        id: 'm-system',
        role: 'system',
        content: 'You are mock system prompt content that should be collapsed by default.'
      },
      ...additionalSnapshot(session)
    ];
  }
  if (session === 'session-restored') {
    return [
      {
        id: 'm-system',
        role: 'system',
        content: 'System prompt.'
      },
      {
        id: 'user-1',
        role: 'user',
        content: 'Show me restored tool call'
      },
      {
        id: 'assistant-1',
        role: 'assistant',
        content: '',
        tool_calls: [
          {
            id: 'call_1',
            type: 'function',
            call_type: 'function',
            function: {
              name: 'bash_exec',
              arguments: '{"command":"ls -la"}'
            }
          }
        ],
        toolCalls: [
          {
            id: 'call_1',
            type: 'function',
            call_type: 'function',
            function: {
              name: 'bash_exec',
              arguments: '{"command":"ls -la"}'
            }
          }
        ]
      },
      {
        id: 'tool-result-1',
        role: 'tool',
        toolCallId: 'call_1',
        toolName: 'bash_exec',
        content: ''
      }
    ];
  }
  if (session !== 'session-1') return additionalSnapshot(session);
  return [...SESSION_ONE_SNAPSHOT, ...additionalSnapshot(session)];
}

const MOCK_ATTACH_BOUNDARY_SEQ = 100;
// Live handoff seq (101) > attach boundary (100) ⇒ navigates;
// persisted/replayed handoff seq (≤100) ⇒ hydrated as history, no re-navigation (models #1803 reload-safety).
const MOCK_LIVE_HANDOFF_SEQ = 101;
const MOCK_PERSISTED_HANDOFF_SEQ = 100;
const pendingLiveHandoffSessions = new Set<string>();

async function emitHandoff(
  controller: ReadableStreamDefaultController<Uint8Array>,
  threadId: string,
  runId: string
) {
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'turn_handoff_requested',
    value: { agent: 'assistant', session_id: null },
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'session_handoff',
    value: { agent: 'assistant', session_id: 'handoff-target', after_seq: MOCK_LIVE_HANDOFF_SEQ },
  }));
  controller.enqueue(encodeSseEvent({ type: 'RUN_FINISHED', threadId, runId }));
  controller.close();
}

function emitSnapshot(
  controller: ReadableStreamDefaultController<Uint8Array>,
  session: string,
  threadId: string,
  runId: string
) {
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'session_attach_boundary',
    value: { attached_seq: MOCK_ATTACH_BOUNDARY_SEQ },
  }));
  controller.enqueue(encodeSseEvent({
    type: 'MESSAGES_SNAPSHOT',
    messages: buildSnapshot(session),
  }));
  const states = controlStates.get(session) || [];
  const isLiveHandoff = pendingLiveHandoffSessions.delete(session);
  for (const state of states) {
    let value = state.value;
    if (state.name === 'session_handoff' && isLiveHandoff) {
      // Live handoff turn emits MOCK_LIVE_HANDOFF_SEQ (101) > attach boundary (100) to navigate.
      // Re-attaches / reloads replay the persisted MOCK_PERSISTED_HANDOFF_SEQ (100 <= 100) as history.
      value = { ...(value as any), after_seq: MOCK_LIVE_HANDOFF_SEQ };
    }
    controller.enqueue(encodeSseEvent({
      type: 'CUSTOM',
      threadId,
      runId,
      name: state.name,
      value,
    }));
  }
  if (!activeSessions.has(session)) {
    controller.enqueue(encodeSseEvent({ type: 'RUN_FINISHED', threadId, runId }));
    controller.close();
  } else {
    let listeners = openRunControllers.get(session);
    if (!listeners) {
      listeners = new Set();
      openRunControllers.set(session, listeners);
    }
    listeners.add({ controller, threadId, runId });
  }
}

interface MockRun {
  controller: ReadableStreamDefaultController<Uint8Array>;
  threadId: string;
  runId: string;
}

async function emitGalleryRun({ controller, threadId, runId }: MockRun) {
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'usage',
    value: {
      input: 100,
      output: 200,
      cached: 50,
      session_label: 'Mock Session',
      context_tokens: 300,
      max_context_tokens: 1000,
      context_percent: 30
    }
  }));
  controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_START',
    toolCallId: 'call_123',
    toolCallName: 'fetch_data',
    parentMessageId: 'assistant-1'
  }));
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'tool_summary',
    value: {
      tool_call_id: 'call_123',
      markdown: 'Fetched **data** from API.'
    }
  }));

  // AG-UI's schemas require `delta` for args and a content frame between
  // text-message start/end. Invalid field names abort the rest of the stream.
  controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_ARGS',
    toolCallId: 'call_123',
    delta: '{"query": "example", "limit": 10}'
  }));
  controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_END',
    toolCallId: 'call_123'
  }));
  controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_RESULT',
    messageId: 'tool-result-123',
    toolCallId: 'call_123',
    content: '{"data": "mock_data", "status": 200}',
    role: 'tool'
  }));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_START',
    messageId: 'assistant-1',
    role: 'assistant'
  }));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_CONTENT',
    messageId: 'assistant-1',
    delta: 'Here is a table:\n\n| Column 1 | Column 2 | Column 3 | Column 4 |\n|---|---|---|---|\n| A | B | C | D |\n\nAnd some code:\n\n```javascript\nconsole.log("hello");\n```\n'
  }));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_END',
    messageId: 'assistant-1'
  }));
  controller.enqueue(encodeSseEvent({ type: 'RUN_FINISHED', threadId, runId }));
  controller.close();
}

interface SubAgentRun extends MockRun {
  session: string;
  userText: string;
}

function nextSubAgentRunIds() {
  const exchangeId = ++subAgentExchangeId;
  return {
    invocationId: `mock-invocation-${exchangeId}`,
    assistantMessageId: `assistant-delegation-${exchangeId}`,
    toolCallId: `call-sub-agent-${exchangeId}`,
    toolResultMessageId: `tool-result-sub-agent-${exchangeId}`,
    finalMessageId: `assistant-final-${exchangeId}`,
  };
}

type SubAgentRunIds = ReturnType<typeof nextSubAgentRunIds>;

function subAgentResultContent(ids: SubAgentRunIds) {
  return JSON.stringify({
    session_id: SUB_AGENT_SESSION_ID,
    response: 'Child task complete.',
    sub_agent: { agent: 'researcher', session_id: SUB_AGENT_SESSION_ID },
    sub_agent_progress: subAgentProgress(ids.invocationId, 'done', 1_250),
  });
}

function emitSubAgentStart(run: MockRun, ids: SubAgentRunIds) {
  run.controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_START',
    toolCallId: ids.toolCallId,
    toolCallName: 'researcher_session_prompt',
    parentMessageId: ids.assistantMessageId,
  }));
  run.controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_ARGS',
    toolCallId: ids.toolCallId,
    delta: JSON.stringify({ message: 'Research this task' }),
  }));
  run.controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId: run.threadId,
    runId: run.runId,
    name: 'sub_agent_started',
    value: {
      agent: 'researcher',
      session_id: SUB_AGENT_SESSION_ID,
      invocation_id: ids.invocationId,
    },
  }));
}

async function emitSubAgentProgress(run: MockRun, ids: SubAgentRunIds) {
  await new Promise((resolve) => setTimeout(resolve, 400));
  run.controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId: run.threadId,
    runId: run.runId,
    name: 'sub_agent_progress',
    value: subAgentProgress(ids.invocationId, 'running', 500),
  }));
  await new Promise((resolve) => setTimeout(resolve, 350));
  run.controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId: run.threadId,
    runId: run.runId,
    name: 'sub_agent_progress',
    value: subAgentProgress(ids.invocationId, 'done', 1_250),
  }));
}

function emitSubAgentResult(run: MockRun, ids: SubAgentRunIds, content: string) {
  run.controller.enqueue(encodeSseEvent({ type: 'TOOL_CALL_END', toolCallId: ids.toolCallId }));
  run.controller.enqueue(encodeSseEvent({
    type: 'TOOL_CALL_RESULT',
    messageId: ids.toolResultMessageId,
    toolCallId: ids.toolCallId,
    content,
    role: 'tool',
  }));
  run.controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_START',
    messageId: ids.finalMessageId,
    role: 'assistant',
  }));
  run.controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_CONTENT',
    messageId: ids.finalMessageId,
    delta: 'Delegation complete.',
  }));
  run.controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_END',
    messageId: ids.finalMessageId,
  }));
  run.controller.enqueue(encodeSseEvent({
    type: 'RUN_FINISHED',
    threadId: run.threadId,
    runId: run.runId,
  }));
}

async function emitSubAgentRun({
  controller,
  threadId,
  runId,
  session,
  userText,
}: SubAgentRun) {
  const ids = nextSubAgentRunIds();
  const resultContent = subAgentResultContent(ids);
  const run = { controller, threadId, runId };
  emitSubAgentStart(run, ids);
  await emitSubAgentProgress(run, ids);
  emitSubAgentResult(run, ids, resultContent);
  persistSubAgentExchange({
    session,
    userText,
    resultContent,
    ...ids,
  });
  controller.close();
}

function createAgUiStream({ session, body }: { session: string; body: any }) {
  const threadId = body?.threadId ?? `thread-${session}`;
  const runId = body?.runId ?? 'r-1';
  const messages = Array.isArray(body?.messages) ? body.messages : [];
  const lastMessage = messages[messages.length - 1];
  // A refresh run carries the already-hydrated transcript. Like the real
  // server, only a trailing user message makes this a prompted run.
  const isPromptlessSubscribe = isPromptlessRun(messages, buildSnapshot(session));
  const userText = lastMessage?.role === 'user'
    ? Array.isArray(lastMessage.content)
      ? lastMessage.content.filter((part: any) => part.type === 'text').map((part: any) => part.text).join('\n').trim()
      : typeof lastMessage.content === 'string'
        ? lastMessage.content
        : ''
    : '';

  // For the "pending" session we intentionally keep the run live (no
  // RUN_FINISHED) so the busy composer + status indicator stay visible. That
  // means the response stream stays open. We must NOT leave it open forever:
  // an unbounded stream leaks the connection between test runs (the next
  // subscribe then never receives its events, e.g. `Running task...`). We
  // auto-close after a bounded window and also clean up on client disconnect
  // via cancel(), so every run gets a fresh, working stream.
  let pendingAutoCloseTimer: ReturnType<typeof setTimeout> | undefined;

  return new ReadableStream({
    async start(controller) {
      // AG-UI stream ordering (matches crates/harnx-serve/src/ag_ui.rs and the
      // @ag-ui/client verifyEvents contract): the FIRST event on any stream MUST
      // be RUN_STARTED. The transcript snapshot is emitted *inside* the run
      // envelope, after RUN_STARTED — never before it.
      controller.enqueue(encodeSseEvent({
        type: 'RUN_STARTED',
        threadId,
        runId,
      }));

      if (isPromptlessSubscribe && session === 'session-pending') {
        await new Promise((resolve) => setTimeout(resolve, 25));
        controller.enqueue(encodeSseEvent({
          type: 'CUSTOM',
          threadId,
          runId,
          name: 'status',
          value: { text: 'Running task...' }
        }));
        // Emit a partial streamed assistant reply so the pending/running
        // screenshot shows in-progress content rather than an empty bubble.
        controller.enqueue(encodeSseEvent({
          type: 'TEXT_MESSAGE_START',
          messageId: 'assistant-pending',
          role: 'assistant'
        }));
        controller.enqueue(encodeSseEvent({
          type: 'TEXT_MESSAGE_CONTENT',
          messageId: 'assistant-pending',
          delta: 'Working on it — analyzing the request and preparing'
        }));
        // Deliberately do NOT send TEXT_MESSAGE_END / RUN_FINISHED while the
        // test observes the running state. Keep the run live for a bounded
        // window (long enough for assertions + screenshot), then close so the
        // connection is released and the next run starts clean. If the client
        // disconnects first, cancel() clears this timer.
        pendingAutoCloseTimer = setTimeout(() => {
          try {
            controller.close();
          } catch {
            // Stream may already be closed/cancelled — ignore.
          }
        }, 8000);
        return;
      }

      if (isPromptlessSubscribe) {
        // Passive subscribe/hydrate ONLY: emit the transcript snapshot, then
        // close the run envelope (mirrors the server's build_promptless_event_stream:
        // RUN_STARTED -> MESSAGES_SNAPSHOT -> RUN_FINISHED). A prompted run below
        // is a pure delta and must NOT emit MESSAGES_SNAPSHOT — applying an empty/
        // stale snapshot mid-run would wipe the optimistic user message + reply.
        emitSnapshot(controller, session, threadId, runId);
        return;
      }

      if (session === 'session-gallery') {
        await emitGalleryRun({ controller, threadId, runId });
        return;
      }

      if (userText === 'handoff now') {
        await emitHandoff(controller, threadId, runId);
        return;
      }

      if (userText === 'delegate to researcher') {
        await emitSubAgentRun({ controller, threadId, runId, session, userText });
        return;
      }

      await new Promise((resolve) => setTimeout(resolve, 25));
      controller.enqueue(encodeSseEvent({
        type: 'CUSTOM',
        threadId,
        runId,
        name: 'status',
        value: { text: 'Mock stream in progress' }
      }));
      await new Promise((resolve) => setTimeout(resolve, 25));
      controller.enqueue(encodeSseEvent({
        type: 'TEXT_MESSAGE_START',
        messageId: 'assistant-1',
        role: 'assistant'
      }));
      await new Promise((resolve) => setTimeout(resolve, 25));
      controller.enqueue(encodeSseEvent({
        type: 'TEXT_MESSAGE_CONTENT',
        messageId: 'assistant-1',
        delta: `Mock streamed reply to: ${userText || 'empty prompt'}`
      }));
      await new Promise((resolve) => setTimeout(resolve, 25));
      controller.enqueue(encodeSseEvent({
        type: 'TEXT_MESSAGE_END',
        messageId: 'assistant-1'
      }));
      controller.enqueue(encodeSseEvent({
        type: 'CUSTOM',
        threadId,
        runId,
        name: 'status',
        value: { text: 'Mock stream finished' }
      }));
      controller.enqueue(encodeSseEvent({
        type: 'RUN_FINISHED',
        threadId,
        runId
      }));
      finishExchange(controller, session, userText);
    },
    cancel() {
      // The client (browser EventSource) disconnected — e.g. the test navigated
      // away or finished. Clear the pending auto-close timer so we don't leak a
      // timer/connection into the next run.
      if (pendingAutoCloseTimer !== undefined) {
        clearTimeout(pendingAutoCloseTimer);
        pendingAutoCloseTimer = undefined;
      }
    }
  });
}

export const happyPathHandlers = [
  http.get('/v1/agents', ({ request }) => {
    const url = new URL(request.url);
    const role = url.searchParams.get('role');

    let agents = [
      { name: 'coding/coder', description: 'Mock coding agent' },
      { name: 'assistant', description: 'Mock assistant agent' }
    ];

    if (role === 'assistant') {
      agents = [{ name: 'coding/coder', description: 'Mock coding agent' }];
    }

    return HttpResponse.json({ data: agents });
  }),

  http.get('/v1/agents/:agent/sessions', () => {
    return HttpResponse.json([
      // Fixed timestamp so session-list screenshots are deterministic without
      // needing to freeze the browser's Date.now (freezing it collides message
      // ids/timestamps in the assistant-ui runtime and drops streamed messages).
      { session_id: 'session-1', updated_at: '2024-01-01T12:00:00.000Z' }, { session_id: 'session-gallery', updated_at: '2024-01-01T12:00:00.000Z' }, { session_id: 'session-pending', updated_at: '2024-01-01T12:00:00.000Z' }, { session_id: 'session-restored', updated_at: '2024-01-01T12:00:00.000Z' }
    ]);
  }),

  http.post('/v1/agents/:agent/sessions', () => {
    return HttpResponse.json({ session_id: 'aMock1' }, { status: 201 });
  }),

  http.get('/v1/agents/:agent/sessions/:session/events', ({ params }) => {
    return new HttpResponse(createSessionEventsStream(String(params.session)), {
      headers: { 'Content-Type': 'text/event-stream' },
    });
  }),

  http.post('/v1/agents/:agent/sessions/:session/attachments', async () => {
    return HttpResponse.json({ attachment_refs: ['cid:mock-attachment'] });
  }),

  http.post('/v1/agents/:agent/sessions/:session', async ({ request, params }) => {
    if (await isRpcRequest(request)) {
      const body = await request.clone().json() as any;
      if (body.method === 'session/cancel') {
        return HttpResponse.json({ jsonrpc: '2.0', result: { cancelled: true }, id: body.id });
      }
      if (body.method === 'session/prompt') {
        const text = body.params?.text || '';
        const session = String(params.session);
        
        if (session === 'session-gallery') {
          persistGalleryExchange(session, text);
          addControlState(session, {
            name: 'usage',
            value: { input: 100, output: 200, cached: 50, cache_write: 0, context_tokens: 300, max_context_tokens: 1000, context_percent: 30 }
          });
          notify(session);
        } else if (text === 'handoff now') {
          // 1. User message
          // The prompt says: "persist source-session handoff control state... keep HANDOFF_TARGET_SNAPSHOT for target hydration"
          // We must persist the user message + the control state
          const persisted = [...(snapshots.get(session) || buildSnapshot(session))];
          persisted.push({ id: `mock-user-handoff`, role: 'user', content: text });
          snapshots.set(session, persisted);
          
          addControlState(session, {
            name: 'session_handoff',
            value: {
              agent: 'assistant',
              session_id: 'handoff-target',
              handoff_tool_call_id: 'mock_handoff_id',
              after_seq: MOCK_PERSISTED_HANDOFF_SEQ,
            }
          });
          pendingLiveHandoffSessions.add(session);
          notify(session);
        } else if (text === 'delegate to researcher') {
          // Staged sub-agent
          activeSessions.add(session);
          const ids = nextSubAgentRunIds();
          
          // Phase 1: start
          const persisted = [...(snapshots.get(session) || buildSnapshot(session))];
          persisted.push(
            { id: `mock-user-${ids.invocationId}`, role: 'user', content: text },
            {
              id: ids.assistantMessageId,
              role: 'assistant',
              content: '',
              toolCalls: [{
                id: ids.toolCallId,
                type: 'function',
                call_type: 'function',
                function: {
                  name: 'researcher_session_prompt',
                  arguments: JSON.stringify({ message: 'Research this task' }),
                },
              }],
            }
          );
          snapshots.set(session, persisted);
          
          addControlState(session, {
            name: 'sub_agent_started',
            value: {
              agent: 'researcher',
              session_id: 'child-session-0001',
              invocation_id: ids.invocationId,
              tool_call_id: ids.toolCallId,
              started_at: new Date().toISOString()
            }
          });
          notify(session);
          
          // Broadcast live progress immediately after notify
          setTimeout(() => {
            broadcastLiveEvent(session, {
              type: 'CUSTOM',
              name: 'sub_agent_progress',
              value: subAgentProgress(ids.invocationId, 'running', 200)
            });
          }, 50);
          
          // Phase 2: finish after delay
          setTimeout(() => {
            activeSessions.delete(session);
            const finalPersisted = [...(snapshots.get(session) || [])];
            finalPersisted.push(
              {
                id: ids.toolResultMessageId,
                role: 'tool',
                toolCallId: ids.toolCallId,
                content: subAgentResultContent(ids)
              },
              {
                id: ids.finalMessageId,
                role: 'assistant',
                content: 'Delegation complete.'
              }
            );
            snapshots.set(session, finalPersisted);
            notify(session);
            
            // close pending runs
            const listeners = openRunControllers.get(session) || new Set();
            for (const { controller, threadId, runId } of listeners) {
              try {
                controller.enqueue(encodeSseEvent({ type: 'RUN_FINISHED', threadId, runId }));
                controller.close();
              } catch (e) {}
            }
            openRunControllers.delete(session);
          }, 1000);
          
        } else {
          persistExchange(session, text, `Mock streamed reply to: ${text || 'empty prompt'}`);
        }
        return HttpResponse.json({ jsonrpc: '2.0', result: { status: 'accepted', run_id: 'mock-run-123' }, id: body.id });
      }
      return HttpResponse.json({
        jsonrpc: '2.0',
        error: { code: -32601, message: 'method not found' },
        id: body.id ?? null,
      }, { status: 404 });
    }

    if (!isSseRequest(request)) {
      return new HttpResponse(null, { status: 406, statusText: 'Not Acceptable' });
    }

    const body = await request.clone().json() as any;
    const stream = createAgUiStream({ session: String(params.session), body });

    return new HttpResponse(stream, {
      headers: {
        'Content-Type': 'text/event-stream'
      }
    });
  }),
];

export const agentsFailHandlers = [
  http.get('/v1/agents', () => {
    return new HttpResponse(null, { status: 500, statusText: 'Internal Server Error' });
  }),
  ...happyPathHandlers
];

export const sessionsFailHandlers = [
  http.get('/v1/agents/:agent/sessions', () => {
    return new HttpResponse(null, { status: 404, statusText: 'Not Found' });
  }),
  ...happyPathHandlers
];

export const sendFailHandlers = [
  http.post('/v1/agents/:agent/sessions/:session', async ({ request }) => {
    if (await isRpcRequest(request)) {
      const body = await request.clone().json() as any;
      if (body.method === 'session/prompt') {
        return HttpResponse.json({
          jsonrpc: '2.0',
          error: { code: -32000, message: 'Simulated sendPrompt error' },
          id: body.id ?? null,
        }, { status: 500 });
      }
      return;
    }

    if (!isSseRequest(request)) {
      return new HttpResponse(null, { status: 406, statusText: 'Not Acceptable' });
    }

    const body = await request.clone().json() as any;
    const messages = Array.isArray(body?.messages) ? body.messages : [];
    if (messages.length > 0) {
      return new HttpResponse(null, { status: 500, statusText: 'Internal Server Error' });
    }

    return new HttpResponse(createAgUiStream({ session: 'session-1', body }), {
      headers: {
        'Content-Type': 'text/event-stream'
      }
    });
  }),
  ...happyPathHandlers
];

export const scenarios = {
  happy: happyPathHandlers,
  agentsFail: agentsFailHandlers,
  sessionsFail: sessionsFailHandlers,
  sendFail: sendFailHandlers,
};

export const handlers = happyPathHandlers;
