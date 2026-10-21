import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';
import { HttpAgent, type AgentSubscriber, type RunAgentResult } from '@ag-ui/client';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { HarnxHttpAgent } from '../ChatProvider';

const summary = { id: 'summary', role: 'assistant', content: 'Earlier work archived.' };
const prompt = { id: 'prompt', role: 'user', content: 'Continue working.' };
const keptRound = {
  id: 'kept', role: 'assistant', content: 'Checking',
  toolCalls: [{ id: 'call', type: 'function', function: { name: 'read', arguments: '{}' } }],
};
const keptResult = { id: 'result', role: 'tool', toolCallId: 'call', content: 'done' };
type WireEvent = { type: string; [key: string]: unknown };
type Callback = (payload: { event: WireEvent }) => unknown;
const callbacks: Record<string, string> = {
  RUN_STARTED: 'onRunStartedEvent', RUN_FINISHED: 'onRunFinishedEvent',
  TEXT_MESSAGE_START: 'onTextMessageStartEvent', TEXT_MESSAGE_CONTENT: 'onTextMessageContentEvent',
  TEXT_MESSAGE_END: 'onTextMessageEndEvent', TOOL_CALL_START: 'onToolCallStartEvent',
  TOOL_CALL_ARGS: 'onToolCallArgsEvent', TOOL_CALL_END: 'onToolCallEndEvent',
  TOOL_CALL_RESULT: 'onToolCallResultEvent', CUSTOM: 'onCustomEvent',
  MESSAGES_SNAPSHOT: 'onMessagesSnapshotEvent',
};

async function streamHarness() {
  let subscriber: AgentSubscriber | undefined;
  let finish!: () => void;
  vi.spyOn(HttpAgent.prototype, 'runAgent').mockImplementation(async (_, next) => {
    subscriber = next;
    await new Promise<void>((resolve) => { finish = resolve; });
    return {} as RunAgentResult;
  });
  const agent = new HarnxHttpAgent({
    url: '/test', onStatus: vi.fn(), onRunFailed: vi.fn(), onUsage: vi.fn(),
    onToolSummary: vi.fn(), onSubAgentEvent: vi.fn(),
  });
  const { result } = renderHook(() => useAgUiRuntime({ agent }));
  await act(async () => {
    void result.current.thread.append('Continue working.');
  });
  await waitFor(() => expect(subscriber).toBeDefined());
  const send = async (event: WireEvent) => {
    const target = subscriber as unknown as Record<string, Callback | undefined>;
    await act(async () => {
      await target.onEvent?.({ event });
      await target[callbacks[event.type]!]?.({ event });
    });
  };
  const done = async () => {
    await send({ type: 'RUN_FINISHED', threadId: 'thread', runId: 'run' });
    await act(async () => { finish(); });
    await waitFor(() => expect(result.current.thread.getState().isRunning).toBe(false));
  };
  await send({ type: 'RUN_STARTED', threadId: 'thread', runId: 'run' });
  return { send, done, messages: () => result.current.thread.getState().messages };
}

async function text(stream: Awaited<ReturnType<typeof streamHarness>>, messageId: string, delta: string) {
  await stream.send({ type: 'TEXT_MESSAGE_START', messageId, role: 'assistant' });
  await stream.send({ type: 'TEXT_MESSAGE_CONTENT', messageId, delta });
}

async function compact(stream: Awaited<ReturnType<typeof streamHarness>>, kept = true) {
  await stream.send({ type: 'CUSTOM', name: 'session_compacting_started', value: {} });
  await stream.send({ type: 'CUSTOM', name: 'session_compacting_completed', value: { outcome: { status: 'compacted' } } });
  await stream.send({ type: 'CUSTOM', name: 'session_compaction', value: { compaction: null } });
  await stream.send({ type: 'MESSAGES_SNAPSHOT', messages: kept ? [summary, prompt, keptRound, keptResult] : [summary] });
}

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

it('snapshot replacement during an active text stream keeps the streaming message', async () => {
  const stream = await streamHarness();
  await text(stream, 'live', 'before ');
  await stream.send({ type: 'MESSAGES_SNAPSHOT', messages: [summary, prompt] });
  expect(stream.messages().find((m) => m.id === 'live')?.content).toMatchObject([{ type: 'text', text: 'before ' }]);
  await stream.send({ type: 'TEXT_MESSAGE_CONTENT', messageId: 'live', delta: 'after' });
  await stream.send({ type: 'TEXT_MESSAGE_END', messageId: 'live' });
  await stream.done();
  expect(stream.messages().map((m) => m.id)).toEqual(['summary', 'prompt', 'live']);
  expect(stream.messages().at(-1)?.content).toMatchObject([{ type: 'text', text: 'before after' }]);
});

it.each([
  { prelude: false, kept: false },
  { prelude: false, kept: true },
  { prelude: true, kept: false },
  { prelude: true, kept: true },
])('continues after compaction without resurrecting tool parts (prelude=$prelude, kept=$kept)', async ({ prelude, kept }) => {
  const stream = await streamHarness();
  if (prelude) {
    await text(stream, 'kept', 'Checking');
    await stream.send({ type: 'TEXT_MESSAGE_END', messageId: 'kept' });
  }
  await stream.send({ type: 'TOOL_CALL_START', toolCallId: 'call', toolCallName: 'read', parentMessageId: 'kept' });
  await stream.send({ type: 'TOOL_CALL_ARGS', toolCallId: 'call', delta: '{}' });
  await stream.send({ type: 'TOOL_CALL_END', toolCallId: 'call' });
  await stream.send({ type: 'TOOL_CALL_RESULT', toolCallId: 'call', content: 'done', messageId: 'result' });
  await compact(stream, kept);
  await text(stream, 'resumed', 'Finished.');
  await stream.send({ type: 'TEXT_MESSAGE_END', messageId: 'resumed' });
  await stream.done();
  expect(stream.messages().map((m) => m.id)).toEqual(kept ? ['summary', 'prompt', 'kept', 'resumed'] : ['summary', 'resumed']);
  expect(stream.messages().at(-1)?.content).toEqual([{ type: 'text', text: 'Finished.' }]);
  expect(stream.messages().flatMap((m) => m.role === 'assistant' ? m.content.filter((part) => part.type === 'tool-call') : [])).toHaveLength(kept ? 1 : 0);
});

it('zero-kept compaction followed by turn end leaves only the summary', async () => {
  const stream = await streamHarness();
  await compact(stream, false);
  await stream.done();
  expect(stream.messages().map((m) => m.id)).toEqual(['summary']);
});
