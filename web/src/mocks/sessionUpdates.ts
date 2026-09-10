const subscribers = new Map<string, Set<ReadableStreamDefaultController>>();
export const snapshots = new Map<string, any[]>();

export const controlStates = new Map<string, any[]>();

export function addControlState(session: string, state: any) {
  const current = controlStates.get(session) ?? [];
  controlStates.set(session, [...current, state]);
}

let messageId = 0;

const channel = typeof window === 'undefined' || typeof window.BroadcastChannel === 'undefined'
  ? undefined
  : new window.BroadcastChannel('harnx-msw-session-updates');

export function notify(session: string) {
  const frame = new TextEncoder().encode(
    `event: session-updated\ndata: ${JSON.stringify({ after_seq: ++messageId })}\n\n`,
  );
  for (const controller of subscribers.get(session) ?? []) controller.enqueue(frame);
}

channel?.addEventListener('message', (event) => {
  const update = event.data as { session?: string; messages?: any[]; controlStates?: any[] };
  if (!update.session || !Array.isArray(update.messages)) return;
  if (update.controlStates) controlStates.set(update.session, update.controlStates);
  snapshots.set(update.session, update.messages);
  notify(update.session);
});

export function additionalSnapshot(session: string): any[] {
  return snapshots.get(session) ?? [];
}

export function persistExchange(session: string, userText: string, reply: string) {
  const persisted = additionalSnapshot(session);
  persisted.push(
    { id: `mock-user-${messageId++}`, role: 'user', content: userText },
    { id: `mock-assistant-${messageId++}`, role: 'assistant', content: reply },
  );
  snapshots.set(session, persisted);
  notify(session);
  channel?.postMessage({ session, messages: persisted, controlStates: controlStates.get(session) });
}


export function persistToolExchange(session: string, userText: string) {
  const persisted = additionalSnapshot(session);
  persisted.push(
    { id: `mock-user-${messageId++}`, role: 'user', content: userText },
    {
      id: `assistant-${messageId++}`,
      role: 'assistant',
      content: '',
      toolCalls: [{
        id: 'call_123',
        type: 'function',
        call_type: 'function',
        function: {
          name: 'fetch_data',
          arguments: '{"query": "example", "limit": 10}',
        },
      }],
    },
    {
      id: `tool-result-${messageId++}`,
      role: 'tool',
      toolCallId: 'call_123',
      content: '{"data": "mock_data", "status": 200}',
    },
  );
  snapshots.set(session, persisted);
  notify(session);
  channel?.postMessage({ session, messages: persisted, controlStates: controlStates.get(session) });
}

interface SubAgentExchange {
  session: string;
  userText: string;
  resultContent: string;
  assistantMessageId: string;
  toolCallId: string;
  toolResultMessageId: string;
  finalMessageId: string;
}

export function persistSubAgentExchange({
  session,
  userText,
  resultContent,
  assistantMessageId,
  toolCallId,
  toolResultMessageId,
  finalMessageId,
}: SubAgentExchange) {
  const persisted = additionalSnapshot(session);
  persisted.push(
    { id: `mock-user-${messageId++}`, role: 'user', content: userText },
    {
      id: assistantMessageId,
      role: 'assistant',
      content: '',
      toolCalls: [{
        id: toolCallId,
        type: 'function',
        call_type: 'function',
        function: {
          name: 'researcher_session_prompt',
          arguments: JSON.stringify({ message: 'Research this task' }),
        },
      }],
    },
    {
      id: toolResultMessageId,
      role: 'tool',
      toolCallId,
      content: resultContent,
    },
    {
      id: finalMessageId,
      role: 'assistant',
      content: 'Delegation complete.',
    },
  );
  snapshots.set(session, persisted);
  notify(session);
  channel?.postMessage({ session, messages: persisted, controlStates: controlStates.get(session) });
}

export function isPromptlessRun(messages: any[], snapshot: readonly { id?: string }[]): boolean {
  const lastMessage = messages.at(-1);
  return lastMessage?.role !== 'user'
    || snapshot.some((message) => message.id === lastMessage.id);
}

export function finishExchange(
  controller: ReadableStreamDefaultController,
  session: string,
  userText: string,
) {
  persistExchange(session, userText, `Mock streamed reply to: ${userText || 'empty prompt'}`);
  controller.close();
}

export function createSessionEventsStream(session: string) {
  let keepOpenTimer: ReturnType<typeof setTimeout> | undefined;
  let streamController: ReadableStreamDefaultController | undefined;
  return new ReadableStream({
    start(controller) {
      streamController = controller;
      const listeners = subscribers.get(session) ?? new Set();
      listeners.add(controller);
      subscribers.set(session, listeners);
      controller.enqueue(new TextEncoder().encode(': connected\n\n'));
      keepOpenTimer = setTimeout(() => {
        listeners.delete(controller);
        controller.close();
      }, 30000);
    },
    cancel() {
      if (keepOpenTimer !== undefined) clearTimeout(keepOpenTimer);
      if (streamController !== undefined) subscribers.get(session)?.delete(streamController);
    },
  });
}
export function persistGalleryExchange(session: string, userText: string) {
  const persisted = additionalSnapshot(session);
  persisted.push(
    { id: `mock-user-${messageId++}`, role: 'user', content: userText },
    {
      id: `assistant-${messageId++}`,
      role: 'assistant',
      content: 'Here is a table:\n\n| Column 1 | Column 2 | Column 3 | Column 4 |\n|---|---|---|---|\n| A | B | C | D |\n\nAnd some code:\n\n```javascript\nconsole.log("hello");\n```\n',
      toolCalls: [{
        id: 'call_123',
        type: 'function',
        call_type: 'function',
        function: {
          name: 'fetch_data',
          arguments: '{"query": "example", "limit": 10}'
        }
      }]
    },
    {
      id: 'tool-result-123',
      role: 'tool',
      toolCallId: 'call_123',
      toolName: 'fetch_data',
      content: '{"data": "mock_data", "status": 200}'
    }
  );
  snapshots.set(session, persisted);
  
  // also add tool_summary to controlStates
  addControlState(session, {
    name: 'tool_summary',
    value: { tool_call_id: 'call_123', markdown: 'Fetched **data** from API.' }
  });
  
  notify(session);
  channel?.postMessage({ session, messages: persisted, controlStates: controlStates.get(session) });
}


export function broadcastLiveEvent(session: string, eventData: any) {
  const frame = new TextEncoder().encode(`event: message\ndata: ${JSON.stringify(eventData)}\n\n`);
  for (const controller of subscribers.get(session) ?? []) {
    try { controller.enqueue(frame); } catch (e) {}
  }
}
