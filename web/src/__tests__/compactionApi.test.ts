import { describe, it, expect, vi, beforeEach } from 'vitest';
import { compactSession, COMPACTION_TIMEOUT_MS } from '../compactionApi';

const fetchMock = vi.fn();
globalThis.fetch = fetchMock as any;

describe('compactionApi', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it('exports a compaction timeout of 15 seconds', () => {
    expect(COMPACTION_TIMEOUT_MS).toBe(15000);
  });

  describe('compactSession', () => {
    it('sends JSON-RPC session/compact and returns submitted result', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          result: {
            status: 'submitted',
            compaction_id: 'compact-123',
          },
        }),
      });

      const result = await compactSession('agent-1', 'session-1');

      expect(result).toEqual({ status: 'submitted', compaction_id: 'compact-123' });
      expect(fetchMock).toHaveBeenCalledWith(
        '/v1/agents/agent-1/sessions/session-1',
        expect.objectContaining({
          method: 'POST',
          signal: expect.any(AbortSignal),
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'session/compact' }),
        })
      );
    });

    it('handles already_in_flight status', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          result: {
            status: 'already_in_flight',
            compaction_id: 'compact-456',
          },
        }),
      });

      const result = await compactSession('agent-1', 'session-1');

      expect(result).toEqual({ status: 'already_in_flight', compaction_id: 'compact-456' });
    });

    it('handles nothing_to_do status with outcome', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          result: {
            status: 'nothing_to_do',
            outcome: { status: 'unchanged', detail: 'no_user_messages' },
          },
        }),
      });

      const result = await compactSession('agent-1', 'session-1');

      expect(result).toEqual({
        status: 'nothing_to_do',
        outcome: { status: 'unchanged', detail: 'no_user_messages' },
      });
    });

    it('throws on HTTP 500 server error', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 500,
        statusText: 'Internal Server Error',
        json: async () => ({}),
      });

      await expect(compactSession('agent-1', 'session-1')).rejects.toThrow(
        'RPC call failed with HTTP 500'
      );
    });

    it('throws on HTTP 400 with error body', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        statusText: 'Bad Request',
        json: async () => ({ error: { code: -32602, message: 'Invalid params' } }),
      });

      await expect(compactSession('agent-1', 'session-1')).rejects.toThrow('RPC Error: Invalid params');
    });

    it('throws on JSON-RPC error response even with HTTP 200', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          error: { code: -32600, message: 'Invalid Request' },
        }),
      });

      await expect(compactSession('agent-1', 'session-1')).rejects.toThrow('RPC Error: Invalid Request');
    });

    it('throws on timeout (TimeoutError)', async () => {
      const timeoutError = new DOMException('The operation was aborted due to timeout', 'TimeoutError');
      fetchMock.mockRejectedValueOnce(timeoutError);

      await expect(compactSession('agent-1', 'session-1')).rejects.toMatchObject({
        name: 'TimeoutError',
      });
    });

    it('respects caller-provided abort signal', async () => {
      const controller = new AbortController();
      let passedSignal: AbortSignal | undefined;

      fetchMock.mockImplementationOnce((_url: any, init: any) => {
        passedSignal = init.signal;
        return new Promise((_, reject) => {
          if (init.signal?.aborted) {
            reject(init.signal.reason);
          } else {
            init.signal?.addEventListener('abort', () => reject(init.signal.reason));
          }
        });
      });

      const promise = compactSession('agent-1', 'session-1', { signal: controller.signal });

      expect(passedSignal).toBeDefined();
      expect(passedSignal?.aborted).toBe(false);

      controller.abort();

      await expect(promise).rejects.toMatchObject({
        name: 'AbortError',
      });
    });

    it('URL-encodes agent and session IDs', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ result: { status: 'submitted', compaction_id: 'c1' } }),
      });

      await compactSession('coding/coder', 'session with spaces');

      expect(fetchMock).toHaveBeenCalledWith(
        '/v1/agents/coding%2Fcoder/sessions/session%20with%20spaces',
        expect.any(Object)
      );
    });
  });
});
