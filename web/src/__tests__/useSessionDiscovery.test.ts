import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act, waitFor } from '@testing-library/react';
import { useSessionDiscovery } from '../useSessionDiscovery';
import * as api from '../api';
import type { SessionRef } from '../types';

vi.mock('../api', () => {
  const mockListSessions = vi.fn(() => Promise.resolve([]));
  const mockCreateSession = vi.fn(() =>
    Promise.resolve({ session_id: 'test-session' }),
  );
  return {
    listSessions: mockListSessions,
    createSession: mockCreateSession,
  };
});

describe('useSessionDiscovery', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it('fetches session list for selected agent', async () => {
    const mockSessions: SessionRef[] = [
      { session_id: 's1', unread: true },
      { session_id: 's2', unread: false },
    ];
    vi.mocked(api.listSessions).mockResolvedValueOnce(mockSessions);

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'test-agent',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toEqual(mockSessions);
    });

    expect(api.listSessions).toHaveBeenCalledWith('test-agent', expect.any(Object));
  });

  it('setSessionUnread updates local session unread state', async () => {
    const mockSessions: SessionRef[] = [
      { session_id: 's1', unread: true },
      { session_id: 's2', unread: false },
    ];
    vi.mocked(api.listSessions).mockResolvedValueOnce(mockSessions);

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'test-agent',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toEqual(mockSessions);
    });

    // Update s2 to unread
    act(() => {
      result.current.setSessionUnread('s2', true);
    });

    expect(result.current.sessions.find((s) => s.session_id === 's2')?.unread).toBe(true);
    expect(result.current.sessions.find((s) => s.session_id === 's1')?.unread).toBe(true);
  });

  it('refreshSessions trigger reconciles session list', async () => {
    const mockSessions1: SessionRef[] = [{ session_id: 's1', unread: true }];
    const mockSessions2: SessionRef[] = [{ session_id: 's1', unread: false }];

    vi.mocked(api.listSessions)
      .mockResolvedValueOnce(mockSessions1)
      .mockResolvedValueOnce(mockSessions2);

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'test-agent',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions[0]?.unread).toBe(true);
    });

    // Manual refresh (simulating reconnect or manual trigger)
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      expect(result.current.sessions[0]?.unread).toBe(false);
    });
  });
});

describe('session reconciliation patterns', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('periodic 30s interval refetches session list', async () => {
    vi.useFakeTimers();

    const mockSessions1: SessionRef[] = [{ session_id: 's1', unread: true }];
    const mockSessions2: SessionRef[] = [{ session_id: 's1', unread: false }];

    vi.mocked(api.listSessions)
      .mockResolvedValueOnce(mockSessions1)
      .mockResolvedValueOnce(mockSessions2);

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'test-agent',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    // Initial mount triggers refreshSessions
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(api.listSessions).toHaveBeenCalledTimes(1);
    expect(result.current.sessions).toEqual(mockSessions1);

    // 30-second periodic reconcile interval fires
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30000);
    });

    expect(api.listSessions).toHaveBeenCalledTimes(2);
    expect(result.current.sessions).toEqual(mockSessions2);
  });
});
