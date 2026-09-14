import { describe, expect, it, vi } from 'vitest';
import { HarnxHttpAgent } from '../ChatProvider';

describe('server run errors', () => {
  it('surfaces a worker startup failure through the real AG-UI client', async () => {
    const message = 'failed to ensure local NATS worker: HARNX_WORKER_BIN points to /missing/harnx-worker, which is not a file';
    const events = [
      { type: 'RUN_STARTED', threadId: 'thread', runId: 'run' },
      { type: 'RUN_ERROR', message },
    ];
    const fetch = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(
      events.map(event => `data: ${JSON.stringify(event)}\n\n`).join(''),
      { headers: { 'Content-Type': 'text/event-stream' } },
    ));
    const onRunFailed = vi.fn();
    const onSubAgentEvent = vi.fn();
    const downstreamError = vi.fn();
    const agent = new HarnxHttpAgent({
      url: 'http://localhost/v1/agents/plain/sessions/session',
      onStatus: vi.fn(),
      onRunFailed,
      onUsage: vi.fn(),
      onToolSummary: vi.fn(),
      onSubAgentEvent,
    });

    try {
      await agent.runAgent({ runId: 'run' }, { onRunErrorEvent: downstreamError });
      expect(onRunFailed).toHaveBeenCalledExactlyOnceWith(message);
      expect(downstreamError).toHaveBeenCalledOnce();
      expect(onSubAgentEvent.mock.calls.filter(([event]) => event.type === 'RUN_ERROR')).toHaveLength(1);
    } finally {
      fetch.mockRestore();
    }
  });
});
