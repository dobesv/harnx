import { act, renderHook, waitFor } from '@testing-library/react';
import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { abandonCancellation, cancel, sessionControl } from '../api';
import { useCancellation } from '../useCancellation';
import type { CancelResult, SessionControlState } from '../types';
vi.mock('../api', () => ({ abandonCancellation: vi.fn(), cancel: vi.fn(), sessionControl: vi.fn() }));
beforeEach(() => vi.resetAllMocks());
afterEach(() => vi.useRealTimers());

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

async function testStopError(error: unknown) {
  vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'running' }, execution_state: 'running', canPrompt: true });
  vi.mocked(cancel).mockRejectedValue(error);
  const { result } = renderHook(() => useCancellation('agent', 'session'));
  await act(async () => { await result.current.stop(); });
  return result.current.phase;
}

async function testResumeAnywayError(error: unknown) {
  vi.mocked(sessionControl).mockResolvedValue({
    state: { status: 'cancel_unconfirmed', cancellation: { cancelled: true, disposition: 'unconfirmed', execution_id: 'execution' } },
  });
  vi.mocked(abandonCancellation).mockRejectedValue(error);
  const { result } = renderHook(() => useCancellation('agent', 'session'));
  await waitFor(() => expect(result.current.phase).toBe('unconfirmed'));
  await act(async () => { await result.current.resumeAnyway(); });
  return result.current.phase;
}

describe('root cancellation state', () => {
  it('lets the server distinguish a progressing cascade from unconfirmed work', async () => {
    vi.useFakeTimers();
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'cancelling', cancellation: { cancelled: true, disposition: 'quiescing' } } });
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await act(async () => { await vi.advanceTimersByTimeAsync(6000); });
    expect(result.current.phase).toBe('stopping');
    vi.mocked(sessionControl).mockRejectedValue(new Error('connection lost'));
    await act(async () => { await vi.advanceTimersByTimeAsync(5500); });
    expect(result.current.phase).toBe('unconfirmed');
  });

  it('does not clear a pending request from an older idle status response', async () => {
    const hydration = deferred<SessionControlState>();
    const acceptance = deferred<CancelResult>();
    vi.mocked(sessionControl).mockReturnValue(hydration.promise);
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    act(() => { void result.current.stop(); });
    expect(result.current.phase).toBe('requesting');
    await act(async () => { hydration.resolve({ state: { status: 'idle' }, canPrompt: true }); });
    expect(result.current.phase).toBe('requesting');
    await act(async () => { acceptance.resolve({ cancelled: true, disposition: 'requested', execution_id: 'execution' }); });
    expect(result.current.phase).toBe('stopping');
  });

  it('hydrates unconfirmed and later converges to cancelled', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'cancel_unconfirmed', cancellation: { cancelled: true, disposition: 'unconfirmed', execution_id: 'execution' } } });
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await waitFor(() => expect(result.current.phase).toBe('unconfirmed'));
    act(() => result.current.observe({ cancelled: true, disposition: 'cancelled', execution_id: 'execution' }));
    expect(result.current.phase).toBe('idle');
  });

  it('abandons only the execution observed in the unconfirmed receipt', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'cancel_unconfirmed', cancellation: { cancelled: true, disposition: 'unconfirmed', execution_id: 'execution' } } });
    vi.mocked(abandonCancellation).mockResolvedValue({ cancelled: true, disposition: 'cancelled', execution_id: 'execution', abandoned: true });
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await waitFor(() => expect(result.current.phase).toBe('unconfirmed'));
    await act(async () => { await result.current.resumeAnyway(); });
    expect(abandonCancellation).toHaveBeenCalledWith('agent', 'session', 'execution');
    expect(result.current.phase).toBe('idle');
  });

  it('keeps a failed request actionable even when the running session permits prompts', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'running' }, execution_state: 'running', canPrompt: true });
    vi.mocked(cancel).mockRejectedValue(new Error('broker unavailable'));
    const { result } = renderHook(() => useCancellation('agent', 'session'));
    await act(async () => { await result.current.stop(); });
    expect(result.current.phase).toBe('failed');
  });

  it('ignores a late receipt after navigating to another session', async () => {
    vi.mocked(sessionControl).mockResolvedValue({ state: { status: 'idle' } });
    const acceptance = deferred<CancelResult>();
    vi.mocked(cancel).mockReturnValue(acceptance.promise);
    const { result, rerender } = renderHook(({ session }) => useCancellation('agent', session), { initialProps: { session: 'old' } });
    act(() => { void result.current.stop(); });
    rerender({ session: 'new' });
    await act(async () => { acceptance.resolve({ cancelled: true, disposition: 'requested' }); });
    expect(result.current.phase).toBe('idle');
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

  it('swallows abort/timeout errors in resumeAnyway() without moving phase to unconfirmed (#1838, #1861)', async () => {
    const phase = await testResumeAnywayError(new DOMException('signal is aborted without reason', 'AbortError'));
    expect(phase).toBe('abandoning');
  });

  it('moves phase to unconfirmed when resumeAnyway() encounters a genuine error', async () => {
    const phase = await testResumeAnywayError(new Error('Network failure'));
    expect(phase).toBe('unconfirmed');
  });
});
