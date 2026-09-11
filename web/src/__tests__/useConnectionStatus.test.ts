import { renderHook, act } from '@testing-library/react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { useConnectionStatus, useRetryCountdownSeconds } from '../useConnectionStatus';
import { connection } from '../connection';

describe('useConnectionStatus', () => {
  it('returns the coordinator snapshot and updates when state changes', () => {
    const { result } = renderHook(() => useConnectionStatus());

    expect(result.current).toEqual({
      status: 'unknown',
      attempt: 0,
      nextRetryAt: null,
      retryingReads: 0,
    });

    act(() => {
      connection.noteTransientTrouble();
    });

    expect(result.current.status).toBe('degraded');

    act(() => {
      connection.noteSuccess();
    });

    expect(result.current.status).toBe('online');
  });
});

describe('useRetryCountdownSeconds', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('returns null when nextRetryAt is null', () => {
    const { result } = renderHook(() => useRetryCountdownSeconds(null));
    expect(result.current).toBeNull();
  });

  it('calculates derived countdown seconds and ticks once per second', () => {
    const now = 1_000_000;
    vi.setSystemTime(now);

    const nextRetryAt = now + 4500; // 4.5 seconds -> ceil is 5s
    const { result } = renderHook(() => useRetryCountdownSeconds(nextRetryAt));

    expect(result.current).toBe(5);

    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(result.current).toBe(4);

    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(result.current).toBe(2);

    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(result.current).toBe(0);
  });

  it('updates immediately when nextRetryAt changes', () => {
    const now = 1_000_000;
    vi.setSystemTime(now);

    const { result, rerender } = renderHook(
      ({ retryAt }: { retryAt: number | null }) => useRetryCountdownSeconds(retryAt),
      { initialProps: { retryAt: null as number | null } }
    );

    expect(result.current).toBeNull();

    rerender({ retryAt: now + 3000 });
    expect(result.current).toBe(3);

    rerender({ retryAt: now + 10000 });
    expect(result.current).toBe(10);

    rerender({ retryAt: null });
    expect(result.current).toBeNull();
  });

  it('cleans up interval on unmount or when nextRetryAt becomes null', () => {
    const clearIntervalSpy = vi.spyOn(globalThis, 'clearInterval');
    const now = 1_000_000;
    vi.setSystemTime(now);

    const { rerender, unmount } = renderHook(
      ({ retryAt }: { retryAt: number | null }) => useRetryCountdownSeconds(retryAt),
      { initialProps: { retryAt: (now + 5000) as number | null } }
    );

    rerender({ retryAt: null });
    expect(clearIntervalSpy).toHaveBeenCalled();

    rerender({ retryAt: now + 5000 });
    unmount();
    expect(clearIntervalSpy).toHaveBeenCalled();
  });
});
