const subscribers = new Map<string, Set<ReadableStreamDefaultController>>();
const snapshots = new Map<string, any[]>();
let messageId = 0;

const channel = typeof window === 'undefined' || typeof window.BroadcastChannel === 'undefined'
  ? undefined
  : new window.BroadcastChannel('harnx-msw-session-updates');

function notify(session: string) {
  const frame = new TextEncoder().encode(
    `event: session-updated\ndata: ${JSON.stringify({ after_seq: ++messageId })}\n\n`,
  );
  for (const controller of subscribers.get(session) ?? []) controller.enqueue(frame);
}

channel?.addEventListener('message', (event) => {
  const update = event.data as { session?: string; messages?: any[] };
  if (!update.session || !Array.isArray(update.messages)) return;
  snapshots.set(update.session, update.messages);
  notify(update.session);
});

export function additionalSnapshot(session: string): any[] {
  return snapshots.get(session) ?? [];
}

function persistExchange(session: string, userText: string, reply: string) {
  const persisted = additionalSnapshot(session);
  persisted.push(
    { id: `mock-user-${messageId++}`, role: 'user', content: userText },
    { id: `mock-assistant-${messageId++}`, role: 'assistant', content: reply },
  );
  snapshots.set(session, persisted);
  notify(session);
  channel?.postMessage({ session, messages: persisted });
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
