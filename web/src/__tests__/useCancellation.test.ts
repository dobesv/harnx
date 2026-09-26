import { act, renderHook } from '@testing-library/react';
import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { cancel } from '../api';
import { useCancellation } from '../useCancellation';
import type { CancelResult } from '../types';
vi.mock('../api', () => ({ cancel: vi.fn() }));
beforeEach(() => vi.resetAllMocks());
afterEach(() => vi.useRealTimers());

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

async function testStopError(error: unknown) {
  vi.mocked(cancel).mockRejectedValue(error);
  const { result } = renderHook(() => useCancellation('agent', 'session'));
  await act(async () => { await result.current.stop(); });
  return result.current.phase;
}

describe('root cancellation state', () => {
  it('stop() moves to requesting then idle on accepted', async () => {
    const acceptance = deferred<CancelResult>();
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    act(() => { void result.current.stop(); });
    expect(result.current.phase).toBe('requesting');
    await act(async () => { acceptance.resolve({ outcome: 'accepted', cancel_seq: 7 }); });
    expect(result.current.phase).toBe('idle');
    expect(cancel).toHaveBeenCalledWith('agent', 'session');
  });

  it('stop() keeps failed actionable on a genuine error', async () => {
    vi.mocked(cancel).mockRejectedValue(new Error('broker unavailable'));
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await act(async () => { await result.current.stop(); });
    expect(result.current.phase).toBe('failed');
  });

  it('ignores a late outcome after navigating to another session', async () => {
    const acceptance = deferred<CancelResult>();
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result, rerender } = renderHook(({ session }) => useCancellation('agent', session), { initialProps: { session: 'old' } });
    act(() => { void result.current.stop(); });
    expect(result.current.phase).toBe('requesting');

    // The session we navigate to starts idle. The outcome that
    // arrives late belongs to the session we left and must not affect the new session.
    rerender({ session: 'new' });
    expect(result.current.phase).toBe('idle');
    await act(async () => { acceptance.resolve({ outcome: 'accepted', cancel_seq: 3 }); });
    expect(result.current.phase).toBe('idle');
  });

  it('swallows abort/timeout errors in stop() without moving phase to failed (#1838, #1861)', async () => {
    const phase = await testStopError(new DOMException('signal is aborted without reason', 'AbortError'));
    expect(phase).toBe('requesting');
  });

  it('swallows TimeoutError in stop() without moving phase to failed (#1861)', async () => {
    const phase = await testStopError(new DOMException('Request timeout', 'TimeoutError'));
    expect(phase).toBe('requesting');
  });

  it('reset() clears requesting phase back to idle', async () => {
    const acceptance = deferred<CancelResult>();
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    act(() => { void result.current.stop(); });
    expect(result.current.phase).toBe('requesting');

    act(() => { result.current.reset?.(); });
    expect(result.current.phase).toBe('idle');
  });
});
