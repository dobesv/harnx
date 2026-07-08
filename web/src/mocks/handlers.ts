import { http, HttpResponse } from 'msw';

export const happyPathHandlers = [
  // list agents
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

  // list sessions
  http.get('/v1/agents/:agent/sessions', () => {
    return HttpResponse.json([
      { session_id: 'session-1', updated_at: new Date().toISOString() }
    ]);
  }),

  http.all('/v1/agents/:agent/sessions/:session', async ({ request }) => {
    let threadId = "t-1";
    try {
      const body = await request.clone().json() as any;
      if (body.threadId) threadId = body.threadId;
    } catch {}

    const stream = new ReadableStream({
      async start(controller) {
        controller.enqueue(new TextEncoder().encode(`event: message\ndata: {"type":"RUN_STARTED","threadId":"${threadId}","runId":"r-1"}\n\n`));
        await new Promise(r => setTimeout(r, 50));
        controller.enqueue(new TextEncoder().encode(`event: message\ndata: {"type":"CUSTOM","threadId":"${threadId}","runId":"r-1","name":"status","value":{"text":"Mock stream finished"}}\n\n`));
        await new Promise(r => setTimeout(r, 50));
        controller.enqueue(new TextEncoder().encode(`event: message\ndata: {"type":"RUN_FINISHED","threadId":"${threadId}","runId":"r-1"}\n\n`));
        controller.close();
      }
    });

    return new HttpResponse(stream, {
      headers: {
        'Content-Type': 'text/event-stream'
      }
    });
  }),

  // rpc
  http.post('/v1/agents/:agent/sessions/:session/rpc', async ({ request }) => {
    const body = await request.clone().json() as any;
    if (body.method === 'session/prompt') {
      return HttpResponse.json({ jsonrpc: "2.0", result: "queued", id: body.id });
    }

    const stream = new ReadableStream({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('event: message\ndata: {"jsonrpc":"2.0","method":"stream/event","params":{"event":{"type":"RUN_STARTED"}}}\n\n'));
        controller.enqueue(new TextEncoder().encode('event: message\ndata: {"jsonrpc":"2.0","method":"stream/event","params":{"event":{"type":"TEXT_MESSAGE_START","message":{"id":"msg-1","role":"assistant","content":[]}}}}\n\n'));
        controller.enqueue(new TextEncoder().encode('event: message\ndata: {"jsonrpc":"2.0","method":"stream/event","params":{"event":{"type":"TEXT_MESSAGE_CHUNK","text":"Hello from mock!"}}}\n\n'));
        controller.enqueue(new TextEncoder().encode('event: message\ndata: {"jsonrpc":"2.0","method":"stream/event","params":{"event":{"type":"TEXT_MESSAGE_END"}}}\n\n'));
        controller.enqueue(new TextEncoder().encode('event: message\ndata: {"jsonrpc":"2.0","method":"stream/event","params":{"event":{"type":"RUN_FINISHED"}}}\n\n'));
        controller.close();
      }
    });

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
  http.post('/v1/agents/:agent/sessions/:session/rpc', () => {
    return new HttpResponse(null, { status: 500, statusText: 'Internal Server Error' });
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
