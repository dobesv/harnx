import { persistExchange, finishExchange } from './sessionUpdates';

export interface MockRun {
  controller: ReadableStreamDefaultController<Uint8Array>;
  threadId: string;
  runId: string;
}

export function encodeSseEvent(data: unknown): Uint8Array {
  return new TextEncoder().encode(`event: message\ndata: ${JSON.stringify(data)}\n\n`);
}

export function finishThinkingExchange(
  controller: ReadableStreamDefaultController,
  session: string,
  userText: string,
) {
  persistExchange(
    session,
    userText,
    '<think>\nThinking through the approach in real time...\n</think>\n\nHere is the final answer after live reasoning.',
  );
  controller.close();
}

export const THINKING_HYDRATED_SNAPSHOT = [
  {
    id: 'think-user-1',
    role: 'user',
    content: 'Solve this problem step by step',
  },
  {
    id: 'think-assistant-1',
    role: 'assistant',
    content: '<think>\nFirst analyze the constraints.\nThen construct the proof.\n</think>\n\nHere is the complete solution to your problem.',
  },
];

export const RESTORED_SESSION_SNAPSHOT = [
  {
    id: 'm-system',
    role: 'system',
    content: 'System prompt.',
  },
  {
    id: 'user-1',
    role: 'user',
    content: 'Show me restored tool call',
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
          arguments: '{"command":"ls -la"}',
        },
      },
    ],
    toolCalls: [
      {
        id: 'call_1',
        type: 'function',
        call_type: 'function',
        function: {
          name: 'bash_exec',
          arguments: '{"command":"ls -la"}',
        },
      },
    ],
  },
  {
    id: 'tool-result-1',
    role: 'tool',
    toolCallId: 'call_1',
    toolName: 'bash_exec',
    content: '',
  },
];

export function getSpecialSnapshot(session: string): any[] | null {
  if (session === 'session-thinking-hydrated') return THINKING_HYDRATED_SNAPSHOT;
  if (session === 'session-restored') return RESTORED_SESSION_SNAPSHOT;
  return null;
}

export async function emitThinkingRun({
  controller,
  threadId,
  runId,
  session,
  userText,
}: MockRun & { session: string; userText: string }) {
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'THINKING_START',
    threadId,
    runId,
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'THINKING_TEXT_MESSAGE_START',
    threadId,
    runId,
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'THINKING_TEXT_MESSAGE_CONTENT',
    delta: 'Thinking through the approach in real time...',
    threadId,
    runId,
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'THINKING_TEXT_MESSAGE_END',
    threadId,
    runId,
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'THINKING_END',
    threadId,
    runId,
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_START',
    messageId: 'assistant-1',
    role: 'assistant',
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_CONTENT',
    messageId: 'assistant-1',
    delta: 'Here is the final answer after live reasoning.',
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_END',
    messageId: 'assistant-1',
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'RUN_FINISHED',
    threadId,
    runId,
  }));
  finishThinkingExchange(controller, session, userText);
}

export async function emitStandardReply({
  controller,
  threadId,
  runId,
  session,
  userText,
}: MockRun & { session: string; userText: string }) {
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'status',
    value: { text: 'Mock stream in progress' },
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_START',
    messageId: 'assistant-1',
    role: 'assistant',
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_CONTENT',
    messageId: 'assistant-1',
    delta: `Mock streamed reply to: ${userText || 'empty prompt'}`,
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'TEXT_MESSAGE_END',
    messageId: 'assistant-1',
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'CUSTOM',
    threadId,
    runId,
    name: 'status',
    value: { text: 'Mock stream finished' },
  }));
  await new Promise((resolve) => setTimeout(resolve, 25));
  controller.enqueue(encodeSseEvent({
    type: 'RUN_FINISHED',
    threadId,
    runId,
  }));
  finishExchange(controller, session, userText);
}
