import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { fetchJsonWithRetry, isAbortError, observedFetch, PermanentError, TransientError } from '../httpClient';
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

describe('httpClient.ts', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe('fetchJsonWithRetry', () => {
    it('retries on 500 and succeeds on second attempt', async () => {
      vi.useFakeTimers();
      const connection = (connectionModule as any).connection;
      connection.waitForRetry.mockResolvedValue(() => {});

      const data = { id: 1, name: 'test' };
      let attemptCount = 0;

      fetchMock.mockImplementation(async () => {
        attemptCount++;
        if (attemptCount === 1) {
          return { ok: false, status: 500, statusText: 'Internal Server Error' };
        }
        return { ok: true, status: 200, json: async () => data };
      });

      const promise = fetchJsonWithRetry('/test');
      await vi.runAllTimersAsync();

      const result = await promise;
      vi.useRealTimers();
      expect(result).toEqual(data);
      expect(attemptCount).toBe(2);
      expect(connection.waitForRetry).toHaveBeenCalledTimes(1);
      expect(connection.noteSuccess).toHaveBeenCalledTimes(1);
    });

    it('retries on 502 (HTTP status) and succeeds on second attempt', async () => {
      vi.useFakeTimers();
      const connection = (connectionModule as any).connection;
      connection.waitForRetry.mockResolvedValue(() => {});

      const data = { items: [] };
      let attemptCount = 0;

      fetchMock.mockImplementation(async () => {
        attemptCount++;
        if (attemptCount === 1) {
          return { ok: false, status: 502, statusText: 'Bad Gateway' };
        }
        return { ok: true, status: 200, json: async () => data };
      });

      const promise = fetchJsonWithRetry('/test');
      await vi.runAllTimersAsync();

      const result = await promise;
      vi.useRealTimers();
      expect(result).toEqual(data);
      expect(attemptCount).toBe(2);
    });

    it('retries on 502 with HTML body and does not treat it as a permanent JSON error', async () => {
      vi.useFakeTimers();
      const connection = (connectionModule as any).connection;
      connection.waitForRetry.mockResolvedValue(() => {});

      const data = { items: ['recovered'] };
      let attemptCount = 0;

      fetchMock.mockImplementation(async () => {
        attemptCount++;
        if (attemptCount === 1) {
          return {
            ok: false,
            status: 502,
            statusText: 'Bad Gateway',
            text: async () => '<html><body>502 Bad Gateway</body></html>',
            json: async () => { throw new SyntaxError('Unexpected token < in JSON at position 0'); },
          };
        }
        return { ok: true, status: 200, json: async () => data };
      });

      const promise = fetchJsonWithRetry('/test');
      await vi.runAllTimersAsync();

      const result = await promise;
      vi.useRealTimers();
      expect(result).toEqual(data);
      expect(attemptCount).toBe(2);
      expect(connection.waitForRetry).toHaveBeenCalledTimes(1);
      expect(connection.noteSuccess).toHaveBeenCalledTimes(1);
    });

    it('retries on network error (fetch rejection) and succeeds on second attempt', async () => {
      vi.useFakeTimers();
      const connection = (connectionModule as any).connection;
      connection.waitForRetry.mockImplementation(async () => () => {});

      const data = { success: true };
      let attemptCount = 0;

      fetchMock.mockImplementation(async () => {
        attemptCount++;
        if (attemptCount === 1) {
          throw new TypeError('Failed to fetch');
        }
        return { ok: true, status: 200, json: async () => data };
      });

      const promise = fetchJsonWithRetry('/test');
      await vi.runAllTimersAsync();

      const result = await promise;
      vi.useRealTimers();
      expect(result).toEqual(data);
      expect(attemptCount).toBe(2);
      expect(connection.waitForRetry).toHaveBeenCalledTimes(1);
    });

    it('rejects immediately with PermanentError on 4xx (no retry)', async () => {
      const connection = (connectionModule as any).connection;

      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 404,
        statusText: 'Not Found',
      });

      await expect(fetchJsonWithRetry('/test')).rejects.toThrow('HTTP error (404): Not Found');
      expect(connection.waitForRetry).not.toHaveBeenCalled();
      expect(connection.noteSuccess).not.toHaveBeenCalled();
    });

    it('rejects with PermanentError on 200 with malformed JSON', async () => {
      const connection = (connectionModule as any).connection;

      fetchMock.mockResolvedValueOnce({
        ok: true,
        status: 200,
        statusText: 'OK',
        json: async () => { throw new SyntaxError('Unexpected token < in JSON at position 0'); },
      });

      await expect(fetchJsonWithRetry('/test')).rejects.toThrow('Malformed JSON in response');
      expect(connection.waitForRetry).not.toHaveBeenCalled();
    });

    it('rethrows caller abort without retry', async () => {
      const connection = (connectionModule as any).connection;

      const controller = new AbortController();
      // Abort before the call to simulate an already-aborted signal
      controller.abort(new DOMException('The user aborted a request', 'AbortError'));

      // Mock rejection - when signal is aborted, fetch will reject
      fetchMock.mockRejectedValueOnce(controller.signal.reason);

      await expect(fetchJsonWithRetry('/test', undefined, { signal: controller.signal })).rejects.toMatchObject({
        name: 'AbortError',
      });
      // Should not call waitForRetry since caller abort happens before retry logic
      expect(connection.waitForRetry.mock.calls.length).toBe(0);
    });

    it('rethrows caller abort from network rejection', async () => {
      const connection = (connectionModule as any).connection;

      const abortError = new DOMException('The user aborted a request', 'AbortError');
      const controller = new AbortController();
      controller.abort(abortError);

      // When signal is already aborted, fetch rejects with the abort reason
      fetchMock.mockRejectedValueOnce(abortError);

      await expect(fetchJsonWithRetry('/test', undefined, { signal: controller.signal })).rejects.toMatchObject({
        name: 'AbortError',
      });
      // When caller aborts, we shouldn't retry
      expect(connection.waitForRetry.mock.calls.length).toBe(0);
    });
  });

  describe('observedFetch', () => {
    it('notes success on HTTP 200', async () => {
      const connection = (connectionModule as any).connection;

      fetchMock.mockResolvedValueOnce({
        ok: true,
        status: 200,
        statusText: 'OK',
      });

      const res = await observedFetch('/test');
      expect(res.status).toBe(200);
      expect(connection.noteSuccess).toHaveBeenCalledTimes(1);
      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });

    it('notes transient trouble on HTTP 500', async () => {
      const connection = (connectionModule as any).connection;

      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 500,
        statusText: 'Internal Server Error',
      });

      const res = await observedFetch('/test');
      expect(res.status).toBe(500);
      expect(connection.noteTransientTrouble).toHaveBeenCalledTimes(1);
      expect(connection.noteSuccess).not.toHaveBeenCalled();
    });

    it('notes transient trouble on HTTP 502', async () => {
      const connection = (connectionModule as any).connection;

      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 502,
        statusText: 'Bad Gateway',
      });

      const res = await observedFetch('/test');
      expect(res.status).toBe(502);
      expect(connection.noteTransientTrouble).toHaveBeenCalledTimes(1);
    });

    it('notes success on HTTP 4xx (no transient trouble)', async () => {
      const connection = (connectionModule as any).connection;

      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 404,
        statusText: 'Not Found',
      });

      const res = await observedFetch('/test');
      expect(res.status).toBe(404);
      // 4xx should not trigger noteSuccess (not >= 500, not ok)
      expect(connection.noteSuccess).not.toHaveBeenCalled();
      expect(connection.noteTransientTrouble).not.toHaveBeenCalled();
    });

    it('notes transient trouble on network error and rethrows original error', async () => {
      const connection = (connectionModule as any).connection;
      const networkError = new TypeError('Failed to fetch');
      fetchMock.mockRejectedValueOnce(networkError);

      await expect(observedFetch('/test')).rejects.toThrow('Failed to fetch');
      expect(connection.noteTransientTrouble).toHaveBeenCalledTimes(1);
    });

    it('rethrows abort error without noting transient trouble', async () => {
      const connection = (connectionModule as any).connection;
      const abortError = new DOMException('The user aborted a request', 'AbortError');
      fetchMock.mockRejectedValueOnce(abortError);

      await expect(observedFetch('/test')).rejects.toMatchObject({
        name: 'AbortError',
      });
      // AbortError should NOT trigger noteTransientTrouble
      expect(connection.noteTransientTrouble.mock.calls.length).toBe(0);
    });

    it('rethrows TimeoutError without noting transient trouble (#1861)', async () => {
      const connection = (connectionModule as any).connection;
      const timeoutError = new DOMException('The operation was aborted due to timeout', 'TimeoutError');
      fetchMock.mockRejectedValueOnce(timeoutError);

      await expect(observedFetch('/test')).rejects.toMatchObject({
        name: 'TimeoutError',
      });
      // TimeoutError should NOT trigger noteTransientTrouble
      expect(connection.noteTransientTrouble.mock.calls.length).toBe(0);
    });

    it('rethrows "signal is aborted without reason" without noting transient trouble (#1838)', async () => {
      const connection = (connectionModule as any).connection;
      const abortError = new DOMException('signal is aborted without reason', 'AbortError');
      fetchMock.mockRejectedValueOnce(abortError);

      await expect(observedFetch('/test')).rejects.toThrow('signal is aborted without reason');
      expect(connection.noteTransientTrouble.mock.calls.length).toBe(0);
    });
  });

  describe('isAbortError', () => {
    it('recognizes standard AbortError DOMException', () => {
      expect(isAbortError(new DOMException('The operation was aborted.', 'AbortError'))).toBe(true);
    });

    it('recognizes TimeoutError DOMException from AbortSignal.timeout (#1861)', () => {
      expect(isAbortError(new DOMException('The operation was aborted due to timeout', 'TimeoutError'))).toBe(true);
    });

    it('recognizes "signal is aborted without reason" (#1838)', () => {
      expect(isAbortError(new DOMException('signal is aborted without reason', 'AbortError'))).toBe(true);
      expect(isAbortError(new Error('signal is aborted without reason'))).toBe(true);
      expect(isAbortError('AbortError: signal is aborted without reason')).toBe(true);
      expect(isAbortError({ message: 'signal is aborted without reason' })).toBe(true);
    });

    it('recognizes other benign abort messages', () => {
      expect(isAbortError(new Error('Fetch is aborted'))).toBe(true);
      expect(isAbortError(new Error('component unmounted'))).toBe(true);
      expect(isAbortError({ name: 'TimeoutError' })).toBe(true);
      expect(isAbortError({ name: 'AbortError' })).toBe(true);
    });

    it('does not classify network or retry errors as abort errors', () => {
      expect(isAbortError(new TypeError('Failed to fetch'))).toBe(false);
      expect(isAbortError(new Error('Network connection failed'))).toBe(false);
      expect(isAbortError(new TransientError('Request timed out'))).toBe(false);
      expect(isAbortError(new PermanentError('HTTP error (404): Not Found'))).toBe(false);
      expect(isAbortError(null)).toBe(false);
      expect(isAbortError(undefined)).toBe(false);
    });
  });
});
