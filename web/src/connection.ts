export type ConnectionStatus = 'unknown' | 'online' | 'reconnecting' | 'degraded';

export type ConnectionSnapshot = Readonly<{
  status: ConnectionStatus;
  attempt: number;
  nextRetryAt: number | null;
  retryingReads: number;
}>;

type TimerHandle = ReturnType<typeof globalThis.setTimeout> | number;

export type ConnectionOptions = {
  now?: () => number;
  random?: () => number;
  setTimeout?: (callback: () => void, delayMs: number) => TimerHandle;
  clearTimeout?: (handle: TimerHandle) => void;
  initialDelayMs?: number;
  maxDelayMs?: number;
};

export type ConnectionCoordinator = {
  subscribe: (listener: () => void) => () => void;
  getSnapshot: () => ConnectionSnapshot;
  /**
   * Reuse one operation-specific signal across rounds, not a per-attempt timeout
   * signal or a caller signal shared by multiple reads. Keep the returned cleanup
   * for the operation's finally block; resolving a wait does not unregister it.
   */
  waitForRetry: (signal: AbortSignal) => Promise<() => void>;
  noteSuccess: () => void;
  noteTransientTrouble: () => void;
};

type RetryWaiter = {
  promise: Promise<() => void>;
  resolve: () => void;
  reject: () => void;
};

function abortError(): DOMException {
  return new DOMException('The operation was aborted.', 'AbortError');
}

function createWaiter(unregister: () => void): RetryWaiter {
  let resolve!: () => void;
  let reject!: () => void;
  const promise = new Promise<() => void>((resolvePromise, rejectPromise) => {
    resolve = () => resolvePromise(unregister);
    reject = () => rejectPromise(abortError());
  });
  return { promise, resolve, reject };
}

function positiveDelay(value: number | undefined, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : fallback;
}

export function createConnectionCoordinator(options: ConnectionOptions = {}): ConnectionCoordinator {
  const now = options.now ?? (() => Date.now());
  const random = options.random ?? (() => Math.random());
  const setTimer = options.setTimeout ?? ((callback, delayMs) => globalThis.setTimeout(callback, delayMs));
  const clearTimer = options.clearTimeout ?? ((handle) => globalThis.clearTimeout(handle));
  const initialDelayMs = positiveDelay(options.initialDelayMs, 1_000);
  const maxDelayMs = positiveDelay(options.maxDelayMs, 60_000);
  const listeners = new Set<() => void>();
  const reads = new Map<AbortSignal, () => void>();
  const waiters = new Map<AbortSignal, RetryWaiter>();
  let timer: TimerHandle | null = null;
  let attempt = 0;
  let nextRetryAt: number | null = null;
  // Recovery observed during another read's retry only becomes visible once all reads finish.
  let lastObservation: Exclude<ConnectionStatus, 'reconnecting'> = 'unknown';
  let snapshot: ConnectionSnapshot = Object.freeze({
    status: 'unknown', attempt: 0, nextRetryAt: null, retryingReads: 0,
  });

  function publish() {
    const retryingReads = reads.size;
    const status = retryingReads > 0 ? 'reconnecting' : lastObservation;
    if (snapshot.status === status && snapshot.attempt === attempt
      && snapshot.nextRetryAt === nextRetryAt && snapshot.retryingReads === retryingReads) return;
    snapshot = Object.freeze({ status, attempt, nextRetryAt, retryingReads });
    for (const listener of [...listeners]) listener();
  }

  function cancelRound() {
    if (timer !== null) clearTimer(timer);
    timer = null;
    nextRetryAt = null;
  }

  function scheduleRound() {
    if (timer !== null) return;
    attempt += 1;
    const baseMs = Math.min(maxDelayMs, initialDelayMs * 2 ** Math.min(attempt - 1, 6));
    const delayMs = baseMs * (0.5 + random());
    nextRetryAt = now() + delayMs;
    timer = setTimer(() => {
      timer = null;
      nextRetryAt = null;
      const roundWaiters = [...waiters.values()];
      waiters.clear();
      // Settle this batch before notifying subscribers, which may start or cancel reads.
      for (const waiter of roundWaiters) waiter.resolve();
      publish();
    }, delayMs);
  }

  function registerRead(signal: AbortSignal): () => void {
    const unregister = () => {
      // An old cleanup must not unregister a later operation reusing the same signal.
      if (reads.get(signal) !== unregister) return;
      reads.delete(signal);
      signal.removeEventListener('abort', unregister);
      const waiter = waiters.get(signal);
      waiters.delete(signal);
      waiter?.reject();
      if (waiters.size === 0) cancelRound();
      if (reads.size === 0) attempt = 0;
      publish();
    };
    reads.set(signal, unregister);
    // Keep cancellation active during the fetch as well as during the backoff wait.
    signal.addEventListener('abort', unregister, { once: true });
    return unregister;
  }

  function waitForRetry(signal: AbortSignal): Promise<() => void> {
    if (signal.aborted) return Promise.reject(abortError());
    const pending = waiters.get(signal);
    if (pending) return pending.promise;
    const unregister = reads.get(signal) ?? registerRead(signal);
    const waiter = createWaiter(unregister);
    waiters.set(signal, waiter);
    lastObservation = 'degraded';
    scheduleRound();
    publish();
    return waiter.promise;
  }

  return {
    subscribe(listener) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    getSnapshot: () => snapshot,
    waitForRetry,
    noteSuccess() {
      lastObservation = 'online';
      publish();
    },
    noteTransientTrouble() {
      lastObservation = 'degraded';
      publish();
    },
  };
}

function singletonOptions(): ConnectionOptions {
  if (!import.meta.env.DEV || typeof window === 'undefined') return {};
  // E2E sets only these delays before module loading. Production ignores the override.
  const overrides = (window as Window & {
    __harnxConnection?: Pick<ConnectionOptions, 'initialDelayMs' | 'maxDelayMs'>;
  }).__harnxConnection;
  return { initialDelayMs: overrides?.initialDelayMs, maxDelayMs: overrides?.maxDelayMs };
}

export const connection = createConnectionCoordinator(singletonOptions());
