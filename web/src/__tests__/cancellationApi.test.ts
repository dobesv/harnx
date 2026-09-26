import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  cancel,
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

  describe('CANCELLATION_TIMEOUT_MS', () => {
    it('is 15 seconds, matching the GET timeout to prevent spurious connection degradation', () => {
      expect(CANCELLATION_TIMEOUT_MS).toBe(15_000);
    });
  });

  describe('cancel', () => {
    it('uses the 15-second timeout and handles -32002 as idle', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        json: async () => ({ error: { code: -32002, message: 'already idle' } }),
      });

      const result = await cancel('agent-1', 'session-1');
      expect(result).toEqual({ outcome: 'idle' });
    });

    it('respects caller abort signal', async () => {
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

      const promise = cancel('agent-1', 'session-1', { signal: controller.signal });

      expect(passedSignal).toBeDefined();
      expect(passedSignal?.aborted).toBe(false);

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

      await expect(cancel('agent-1', 'session-1')).rejects.toMatchObject({
        name: 'TimeoutError',
      });

      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });

    it('does NOT flip connection status to degraded on "signal is aborted without reason" (#1838)', async () => {
      const connection = (connectionModule as any).connection;
      const abortError = new DOMException('signal is aborted without reason', 'AbortError');
      fetchMock.mockRejectedValueOnce(abortError);

      await expect(cancel('agent-1', 'session-1')).rejects.toThrow(
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
        json: async () => { throw new Error('no body'); },
      });

      await expect(cancel('agent-1', 'session-1')).rejects.toThrow('RPC call failed with HTTP 500');
      expect(connection.noteTransientTrouble).toHaveBeenCalledTimes(1);
    });
  });
});
