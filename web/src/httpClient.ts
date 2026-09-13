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

const ABORT_ERROR_NAMES = new Set(['AbortError', 'TimeoutError']);
const BENIGN_ABORT_PATTERNS = [
  'signal is aborted without reason',
  'AbortError',
  'TimeoutError',
  'Fetch is aborted',
  'component unmounted',
  'The operation was aborted',
  'The user aborted a request',
];

function isAbortName(name?: string): boolean {
  return Boolean(name && ABORT_ERROR_NAMES.has(name));
}

function matchesBenignPattern(message: string): boolean {
  return BENIGN_ABORT_PATTERNS.some((pattern) => message.includes(pattern));
}

function extractErrorNameAndMessage(err: unknown): { name?: string; message?: string } {
  if (typeof err === 'string') {
    return { message: err };
  }
  if (err instanceof Error) {
    return { name: err.name, message: err.message };
  }
  if (err && typeof err === 'object') {
    const obj = err as Record<string, unknown>;
    const name = typeof obj.name === 'string' ? obj.name : undefined;
    const message = typeof obj.message === 'string' ? obj.message : undefined;
    return { name, message };
  }
  return {};
}

function isRecognizedNetworkError(err: unknown): boolean {
  if (!err) return true;
  if (err instanceof TransientError) return true;
  if (err instanceof PermanentError) return true;
  return false;
}

export function isAbortError(err: unknown): boolean {
  if (isRecognizedNetworkError(err)) {
    return false;
  }
  const { name, message } = extractErrorNameAndMessage(err);
  if (isAbortName(name)) {
    return true;
  }
  if (!message) {
    return false;
  }
  return matchesBenignPattern(message);
}

function toFetchFailureError(isTimeout: boolean, operationSignal: AbortSignal, err: unknown): Error {
  if (isTimeout) {
    return new TransientError('Request timed out', { cause: err });
  }
  if (operationSignal.aborted) {
    return (operationSignal.reason ?? new DOMException('The operation was aborted.', 'AbortError')) as Error;
  }
  return new TransientError('Network connection failed', { cause: err });
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
    throw toFetchFailureError(isTimeout, operationSignal, err);
  } finally {
    if (timeoutId !== null) clearTimeout(timeoutId);
    operationSignal.removeEventListener('abort', abortHandler);
  }
}

function extractBodyErrorString(body: any): string | undefined {
  if (typeof body?.error === 'string') return body.error;
  if (typeof body?.error?.message === 'string') return body.error.message;
  return undefined;
}

async function extractHttpErrorDetail(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as any;
    const detail = extractBodyErrorString(body);
    if (detail) return detail;
  } catch {
    // Keep status text if body not JSON
  }
  return res.statusText;
}

async function validateHttpStatus(res: Response): Promise<void> {
  if (res.status >= 500) {
    throw new TransientError(`Server error (${res.status}): ${res.statusText}`);
  }
  if (!res.ok) {
    const detail = await extractHttpErrorDetail(res);
    throw new PermanentError(`HTTP error (${res.status}): ${detail}`);
  }
}

function rethrowRecognizedHttpError(err: unknown): void {
  if (err instanceof TransientError) throw err;
  if (err instanceof PermanentError) throw err;
}

function defaultJsonParse<T>(res: Response): Promise<T> {
  return res.json() as Promise<T>;
}

async function parseResponseBody<T>(
  res: Response,
  parse?: (res: Response) => Promise<T>
): Promise<T> {
  try {
    if (parse) {
      return await parse(res);
    }
    return await defaultJsonParse<T>(res);
  } catch (parseErr) {
    rethrowRecognizedHttpError(parseErr);
    throw new PermanentError('Malformed JSON in response', { cause: parseErr });
  }
}

async function handleRetryOrRethrow(
  err: unknown,
  operationSignal: AbortSignal
): Promise<() => void> {
  if (operationSignal.aborted || isAbortError(err)) {
    throw err;
  }
  if (err instanceof PermanentError) {
    throw err;
  }
  if (err instanceof TransientError) {
    return await connection.waitForRetry(operationSignal);
  }
  throw err;
}

export type FetchJsonOptions<T> = {
  signal?: AbortSignal;
  parse?: (res: Response) => Promise<T>;
};

function createCallerAbortError(callerSignal?: AbortSignal): DOMException {
  return callerSignal?.reason ?? new DOMException('The operation was aborted.', 'AbortError');
}

function checkOperationAborted(operationSignal: AbortSignal): void {
  if (operationSignal.aborted) {
    throw operationSignal.reason ?? new DOMException('Aborted', 'AbortError');
  }
}

async function executeAttempt<T>(
  input: RequestInfo | URL,
  init: RequestInit | undefined,
  options: FetchJsonOptions<T> | undefined,
  operationSignal: AbortSignal
): Promise<T> {
  checkOperationAborted(operationSignal);
  const res = await fetchWithTimeout(input, init, GET_TIMEOUT_MS, operationSignal);
  await validateHttpStatus(res);
  const data = await parseResponseBody<T>(res, options?.parse);
  connection.noteSuccess();
  return data;
}

function createOperationSignal(callerSignal?: AbortSignal): {
  operationSignal: AbortSignal;
  cleanup: () => void;
} {
  if (callerSignal?.aborted) {
    throw createCallerAbortError(callerSignal);
  }
  const operationController = new AbortController();
  const onCallerAbort = () => operationController.abort(callerSignal?.reason);
  callerSignal?.addEventListener('abort', onCallerAbort);
  return {
    operationSignal: operationController.signal,
    cleanup: () => callerSignal?.removeEventListener('abort', onCallerAbort),
  };
}

export async function fetchJsonWithRetry<T>(
  input: RequestInfo | URL,
  init?: RequestInit,
  options?: FetchJsonOptions<T>
): Promise<T> {
  const { operationSignal, cleanup } = createOperationSignal(options?.signal);
  let unregister: (() => void) | undefined;

  try {
    // eslint-disable-next-line no-constant-condition
    while (true) {
      try {
        return await executeAttempt<T>(input, init, options, operationSignal);
      } catch (err) {
        unregister = await handleRetryOrRethrow(err, operationSignal);
      }
    }
  } finally {
    unregister?.();
    cleanup();
  }
}

function notifyResponseStatus(res: Response): void {
  if (res.status >= 500) {
    connection.noteTransientTrouble();
    return;
  }
  if (res.ok) {
    connection.noteSuccess();
  }
}

function handleObservedFetchError(err: unknown): never {
  if (!isAbortError(err)) {
    connection.noteTransientTrouble();
  }
  throw err;
}

export async function observedFetch(
  input: RequestInfo | URL,
  init?: RequestInit
): Promise<Response> {
  try {
    const res = await fetch(input, init);
    notifyResponseStatus(res);
    return res;
  } catch (err) {
    handleObservedFetchError(err);
  }
}
