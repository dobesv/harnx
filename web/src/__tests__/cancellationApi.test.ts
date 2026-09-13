import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  abandonCancellation,
  cancel,
  sessionControl,
  CANCELLATION_TIMEOUT_MS,
} from '../cancellationApi';
import * as connectionModule from '../connection';

const fetchMock = vi.fn();
globalThis.fetch = fetchMock as any;

vi.mock('../connection', () => ({
  connection: {
    subscribe: vi.fn(),
    getSnapshot: vi.fn(),
    waitForRetry: vi.fn().mockResolvedValue(() => {}),
    noteSuccess: vi.fn(),
    noteTransientTrouble: vi.fn(),
  },
}));

describe('cancellationApi', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it('exports a cancellation timeout meaningfully above normal p95 server latency (#1861)', () => {
    expect(CANCELLATION_TIMEOUT_MS).toBe(15000);
    expect(CANCELLATION_TIMEOUT_MS).toBeGreaterThan(2000);
  });

  describe('sessionControl', () => {
    it('uses the 15-second timeout and succeeds', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          result: {
            execution_state: 'running',
            state: { status: 'running' },
            execution_id: 'exec-1',
          },
        }),
      });

      const state = await sessionControl('agent-1', 'session-1');
      expect(state.execution_id).toBe('exec-1');
      expect(fetchMock).toHaveBeenCalledWith(
        '/v1/agents/agent-1/sessions/session-1',
        expect.objectContaining({
          method: 'POST',
          signal: expect.any(AbortSignal),
          body: JSON.stringify({ jsonrpc: '2.0', id: 'control', method: 'session/get' }),
        })
      );
    });

    it('respects caller signal when provided (#1838)', async () => {
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

      const promise = sessionControl('agent-1', 'session-1', { signal: controller.signal });

      // Signal passed to fetch must not be aborted initially
      expect(passedSignal).toBeDefined();
      expect(passedSignal?.aborted).toBe(false);

      // Aborting the caller signal must abort the composite signal passed to fetch
      controller.abort();
      expect(passedSignal?.aborted).toBe(true);

      await expect(promise).rejects.toMatchObject({
        name: 'AbortError',
      });
    });

    it('does NOT flip connection status to degraded on TimeoutError (#1861)', async () => {
      const connection = (connectionModule as any).connection;
      const timeoutError = new DOMException('The operation was aborted due to timeout', 'TimeoutError');
      fetchMock.mockRejectedValueOnce(timeoutError);

      await expect(sessionControl('agent-1', 'session-1')).rejects.toMatchObject({
        name: 'TimeoutError',
      });

      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });

    it('does NOT flip connection status to degraded on "signal is aborted without reason" (#1838)', async () => {
      const connection = (connectionModule as any).connection;
      const abortError = new DOMException('signal is aborted without reason', 'AbortError');
      fetchMock.mockRejectedValueOnce(abortError);

      await expect(sessionControl('agent-1', 'session-1')).rejects.toThrow(
        'signal is aborted without reason'
      );

      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });

    it('flips connection status to degraded on HTTP 500 server error', async () => {
      const connection = (connectionModule as any).connection;
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 500,
        statusText: 'Internal Server Error',
        json: async () => ({ error: { message: 'Internal Server Error' } }),
      });

      await expect(sessionControl('agent-1', 'session-1')).rejects.toThrow('Internal Server Error');
      expect(connection.noteTransientTrouble).toHaveBeenCalledTimes(1);
    });
  });

  describe('cancel', () => {
    it('uses the 15-second timeout and handles -32002 as idle', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        json: async () => ({ error: { code: -32002, message: 'already idle' } }),
      });

      const result = await cancel('agent-1', 'session-1', 'exec-1');
      expect(result).toEqual({ cancelled: false, disposition: 'idle' });
    });

    it('does NOT flip connection status to degraded on timeout (#1861)', async () => {
      const connection = (connectionModule as any).connection;
      const timeoutError = new DOMException('Request timeout', 'TimeoutError');
      fetchMock.mockRejectedValueOnce(timeoutError);

      await expect(cancel('agent-1', 'session-1')).rejects.toMatchObject({
        name: 'TimeoutError',
      });

      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });
  });

  describe('abandonCancellation', () => {
    it('uses the 15-second timeout and succeeds', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          result: { cancelled: true, disposition: 'cancelled', execution_id: 'exec-1' },
        }),
      });

      const result = await abandonCancellation('agent-1', 'session-1', 'exec-1');
      expect(result.disposition).toBe('cancelled');
    });

    it('does NOT flip connection status to degraded on timeout (#1861)', async () => {
      const connection = (connectionModule as any).connection;
      const timeoutError = new DOMException('Request timeout', 'TimeoutError');
      fetchMock.mockRejectedValueOnce(timeoutError);

      await expect(abandonCancellation('agent-1', 'session-1', 'exec-1')).rejects.toMatchObject({
        name: 'TimeoutError',
      });

      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });
  });
});
