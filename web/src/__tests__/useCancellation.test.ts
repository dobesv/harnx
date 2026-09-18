import { act, renderHook, waitFor } from '@testing-library/react';
import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { cancel, sessionControl } from '../api';
import { useCancellation } from '../useCancellation';
import type { CancelResult } from '../types';
vi.mock('../api', () => ({ cancel: vi.fn(), sessionControl: vi.fn() }));
beforeEach(() => vi.resetAllMocks());
afterEach(() => vi.useRealTimers());

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

async function testStopError(error: unknown) {
  vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'running' }, canPrompt: true });
  vi.mocked(cancel).mockRejectedValue(error);
  const { result } = renderHook(() => useCancellation('agent', 'session'));
  await act(async () => { await result.current.stop(); });
  return result.current.phase;
}

describe('root cancellation state', () => {
  it('stop() moves to requesting then idle on accepted', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'running' } });
    const acceptance = deferred<CancelResult>();
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    act(() => { void result.current.stop(); });
    expect(result.current.phase).toBe('requesting');
    // A hydration answered while the request is in flight must not clear it.
    await act(async () => { await Promise.resolve(); });
    expect(result.current.phase).toBe('requesting');
    await act(async () => { acceptance.resolve({ outcome: 'accepted', cancel_seq: 7 }); });
    expect(result.current.phase).toBe('idle');
    expect(cancel).toHaveBeenCalledWith('agent', 'session');
  });

  it('stop() keeps failed actionable on a genuine error', async () => {
    vi.useFakeTimers();
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'running' }, canPrompt: true });
    vi.mocked(cancel).mockRejectedValue(new Error('broker unavailable'));
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await act(async () => { await result.current.stop(); });
    expect(result.current.phase).toBe('failed');
    // Hydrating a session that is still running must leave the retry offered.
    await act(async () => { await vi.advanceTimersByTimeAsync(1500); });
    expect(result.current.phase).toBe('failed');
  });

  it('ignores a late outcome after navigating to another session', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'idle' } });
    const acceptance = deferred<CancelResult>();
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result, rerender } = renderHook(({ session }) => useCancellation('agent', session), { initialProps: { session: 'old' } });
    act(() => { void result.current.stop(); });

    // The session we navigate to is mid-interrupt of its own. The outcome that
    // arrives late belongs to the session we left and must not clear it.
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'interrupting' } });
    rerender({ session: 'new' });
    await waitFor(() => expect(result.current.phase).toBe('requesting'));
    await act(async () => { acceptance.resolve({ outcome: 'accepted', cancel_seq: 3 }); });
    expect(result.current.phase).toBe('requesting');
  });

  it('hydrates a session whose interrupt is still being appended as requesting', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'interrupting' } });
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await waitFor(() => expect(result.current.phase).toBe('requesting'));
  });

  it('aborts the in-flight sessionControl call on unmount', () => {
    let capturedSignal: AbortSignal | undefined;
    vi.mocked(sessionControl).mockImplementation((_agent, _session, options) => {
      capturedSignal = options?.signal;
      return new Promise(() => {});
    });

    const { unmount } = renderHook(() => useCancellation('agent', 'session'));
    expect(capturedSignal).toBeDefined();
    expect(capturedSignal?.aborted).toBe(false);

    unmount();
    expect(capturedSignal?.aborted).toBe(true);
  });

  it('swallows abort/timeout errors in stop() without moving phase to failed (#1838, #1861)', async () => {
    const phase = await testStopError(new DOMException('signal is aborted without reason', 'AbortError'));
    expect(phase).toBe('requesting');
  });

  it('swallows TimeoutError in stop() without moving phase to failed (#1861)', async () => {
    const phase = await testStopError(new DOMException('Request timeout', 'TimeoutError'));
    expect(phase).toBe('requesting');
  });
});
