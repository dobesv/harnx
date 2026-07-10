import { http, HttpResponse } from 'msw';

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

function buildSnapshot(session: string) {
  if (session === 'session-gallery') {
    return [
      {
        id: 'm-system',
        role: 'system',
        content: 'You are mock system prompt content that should be collapsed by default.'
      }
    ];
  }
  if (session !== 'session-1') return [];
  return [
    {
      id: 'm-system',
      role: 'system',
      content: 'You are mock system prompt content that should be collapsed by default.'
    },
    {
      id: 'm1',
      role: 'assistant',
      content: 'Hello from mock session'
    }
  ];
}

function createAgUiStream({ session, body }: { session: string; body: any }) {
  const threadId = body?.threadId ?? `thread-${session}`;
  const runId = body?.runId ?? 'r-1';
  const messages = Array.isArray(body?.messages) ? body.messages : [];
  const isPromptlessSubscribe = messages.length === 0;
  const lastMessage = messages[messages.length - 1];
  const userText = lastMessage?.role === 'user'
    ? Array.isArray(lastMessage.content)
      ? lastMessage.content.filter((part: any) => part.type === 'text').map((part: any) => part.text).join('\n').trim()
      : typeof lastMessage.content === 'string'
        ? lastMessage.content
        : ''
    : '';

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

      if (isPromptlessSubscribe) {
        // Passive subscribe/hydrate ONLY: emit the transcript snapshot, then
        // close the run envelope (mirrors the server's build_promptless_event_stream:
        // RUN_STARTED -> MESSAGES_SNAPSHOT -> RUN_FINISHED). A prompted run below
        // is a pure delta and must NOT emit MESSAGES_SNAPSHOT — applying an empty/
        // stale snapshot mid-run would wipe the optimistic user message + reply.
        controller.enqueue(encodeSseEvent({
          type: 'MESSAGES_SNAPSHOT',
          messages: buildSnapshot(session),
        }));
        controller.enqueue(encodeSseEvent({
          type: 'RUN_FINISHED',
          threadId,
          runId,
        }));
        controller.close();
        return;
      }

      if (session === 'session-pending') {
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
        // Deliberately never send TEXT_MESSAGE_END / RUN_FINISHED: the run
        // stays live so the status indicator and busy composer remain visible.
        return;
      }
      
      if (session === 'session-gallery') {
        await new Promise((resolve) => setTimeout(resolve, 25));
        // Tool call round first (self-contained: start -> args -> end -> result).
        controller.enqueue(encodeSseEvent({
          type: 'TOOL_CALL_START',
          toolCallId: 'call_123',
          toolCallName: 'fetch_data',
          parentMessageId: 'assistant-1'
        }));
        // NOTE: AG-UI client (@ag-ui/core Zod schemas) require specific field
        // names. TOOL_CALL_ARGS uses `delta` (NOT `argsText`) — a wrong field
        // makes the client's EventSchemas.parse() throw and aborts the whole
        // SSE stream before any later text renders.
        controller.enqueue(encodeSseEvent({
          type: 'TOOL_CALL_ARGS',
          toolCallId: 'call_123',
          delta: '{"query": "example", "limit": 10}'
        }));
        controller.enqueue(encodeSseEvent({
          type: 'TOOL_CALL_END',
          toolCallId: 'call_123'
        }));
        // TOOL_CALL_RESULT requires `messageId` + `content` (NOT `result`).
        controller.enqueue(encodeSseEvent({
          type: 'TOOL_CALL_RESULT',
          messageId: 'tool-result-123',
          toolCallId: 'call_123',
          content: '{"data": "mock_data", "status": 200}',
          role: 'tool'
        }));
        // Assistant text message MUST include TEXT_MESSAGE_CONTENT between
        // START and END — omitting it produces an empty message that the AG-UI
        // client rejects with a Zod "delta Required" validation error.
        controller.enqueue(encodeSseEvent({
          type: 'TEXT_MESSAGE_START',
          messageId: 'assistant-1',
          role: 'assistant'
        }));
        controller.enqueue(encodeSseEvent({
          type: 'TEXT_MESSAGE_CONTENT',
          messageId: 'assistant-1',
          delta: 'Here is the data I fetched for you. The request returned status 200 with the example records you asked for.'
        }));
        controller.enqueue(encodeSseEvent({
          type: 'TEXT_MESSAGE_END',
          messageId: 'assistant-1'
        }));
        controller.enqueue(encodeSseEvent({
          type: 'RUN_FINISHED',
          threadId,
          runId,
        }));
        controller.close();
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
      controller.close();
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
      { session_id: 'session-1', updated_at: '2024-01-01T12:00:00.000Z' }, { session_id: 'session-gallery', updated_at: '2024-01-01T12:00:00.000Z' }, { session_id: 'session-pending', updated_at: '2024-01-01T12:00:00.000Z' }
    ]);
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