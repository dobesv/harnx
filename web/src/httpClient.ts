import { connection } from './connection';

/** Network-level or server-side failure that may succeed on retry. */
export class TransientError extends Error {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message);
    this.name = 'TransientError';
    this.cause = options?.cause;
  }
}

/** Client error or malformed response that will not succeed on retry. */
export class PermanentError extends Error {
  constructor(message: string, options?: { cause?: unknown }) {
    super(message);
    this.name = 'PermanentError';
    this.cause = options?.cause;
  }
}

const GET_TIMEOUT_MS = 15000;

export function isAbortError(err: unknown): boolean {
  return (
    (err instanceof Error && err.name === 'AbortError') ||
    (typeof err === 'object' && err !== null && (err as { name?: string }).name === 'AbortError')
  );
}

async function fetchWithTimeout(
  input: RequestInfo | URL,
  init: RequestInit | undefined,
  timeoutMs: number,
  operationSignal: AbortSignal
): Promise<Response> {
  if (operationSignal.aborted) {
    throw operationSignal.reason ?? new DOMException('The operation was aborted.', 'AbortError');
  }

  const controller = new AbortController();
  let timeoutId: ReturnType<typeof setTimeout> | null = null;
  let isTimeout = false;

  const abortHandler = () => {
    controller.abort(operationSignal.reason);
  };
  operationSignal.addEventListener('abort', abortHandler, { once: true });

  if (timeoutMs > 0) {
    timeoutId = setTimeout(() => {
      isTimeout = true;
      controller.abort(new DOMException('Request timeout', 'TimeoutError'));
    }, timeoutMs);
  }

  try {
    return await fetch(input, { ...init, signal: controller.signal });
  } catch (err) {
    if (isTimeout) {
      throw new TransientError('Request timed out', { cause: err });
    }
    if (operationSignal.aborted) {
      throw operationSignal.reason ?? new DOMException('The operation was aborted.', 'AbortError');
    }
    // Fetch rejection (TypeError / connection refused / DNS failure / etc.)
    throw new TransientError('Network connection failed', { cause: err });
  } finally {
    if (timeoutId !== null) clearTimeout(timeoutId);
    operationSignal.removeEventListener('abort', abortHandler);
  }
}

export type FetchJsonOptions<T> = {
  signal?: AbortSignal;
  parse?: (res: Response) => Promise<T>;
};

export async function fetchJsonWithRetry<T>(
  input: RequestInfo | URL,
  init?: RequestInit,
  options?: FetchJsonOptions<T>
): Promise<T> {
  const callerSignal = options?.signal;

  if (callerSignal?.aborted) {
    throw callerSignal.reason ?? new DOMException('The operation was aborted.', 'AbortError');
  }

  // Operation-specific AbortController linked to caller's signal
  const operationController = new AbortController();
  const onCallerAbort = () => operationController.abort();
  callerSignal?.addEventListener('abort', onCallerAbort);
  const operationSignal = operationController.signal;

  let unregister: (() => void) | undefined;

  try {
    // eslint-disable-next-line no-constant-condition
    while (true) {
      if (operationSignal.aborted) {
        throw operationSignal.reason ?? new DOMException('Aborted', 'AbortError');
      }

      try {
        const res = await fetchWithTimeout(input, init, GET_TIMEOUT_MS, operationSignal);

        // Status classification BEFORE body parse
        if (res.status >= 500) {
          throw new TransientError(`Server error (${res.status}): ${res.statusText}`);
        }
        if (!res.ok) {
          // Permanent error (4xx): try to get detail from body
          let detail = res.statusText;
          try {
            const body = await res.json() as any;
            detail = typeof body.error === 'string' ? body.error : body.error?.message || detail;
          } catch {
            // Keep status text if body not JSON
          }
          throw new PermanentError(`HTTP error (${res.status}): ${detail}`);
        }

        // 2xx response: parse JSON or custom parse
        let data: T;
        try {
          data = options?.parse ? await options.parse(res) : await res.json() as T;
        } catch (parseErr) {
          if (parseErr instanceof TransientError || parseErr instanceof PermanentError) {
            throw parseErr;
          }
          throw new PermanentError('Malformed JSON in response', { cause: parseErr });
        }

        connection.noteSuccess();
        return data;
      } catch (err) {
        // Caller abort: rethrow immediately
        if (operationSignal.aborted || isAbortError(err)) {
          throw err;
        }

        // Permanent error: reject immediately (no retry)
        if (err instanceof PermanentError) {
          throw err;
        }

        // Transient error: await backoff and continue loop
        if (err instanceof TransientError) {
          unregister = await connection.waitForRetry(operationSignal);
          // Loop continues to next attempt
          continue;
        }

        // Any unexpected error is rethrown
        throw err;
      }
    }
  } finally {
    unregister?.();
    callerSignal?.removeEventListener('abort', onCallerAbort);
  }
}

export async function observedFetch(
  input: RequestInfo | URL,
  init?: RequestInit
): Promise<Response> {
  try {
    const res = await fetch(input, init);

    // Transport succeeded; classify based on status
    if (res.status >= 500) {
      connection.noteTransientTrouble();
    } else if (res.ok) {
      connection.noteSuccess();
    }
    // Return response unchanged for caller to handle
    return res;
  } catch (err) {
    // Caller abort: rethrow as-is
    if (isAbortError(err)) {
      throw err;
    }
    // Network error: note trouble and rethrow original error unchanged
    connection.noteTransientTrouble();
    throw err;
  }
}
