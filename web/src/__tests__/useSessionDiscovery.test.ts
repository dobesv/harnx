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

describe('pagination and incremental loading', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('fetches first page with limit=50 and captures next_cursor', async () => {
    vi.mocked(api.listSessions).mockResolvedValueOnce({
      sessions: [{ session_id: 's1' }, { session_id: 's2' }],
      next_cursor: 'cur-page-2',
    });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(2);
    });

    expect(api.listSessions).toHaveBeenCalledWith('agent-1', expect.objectContaining({
      limit: 50,
    }));
    expect(result.current.hasMore).toBe(true);
    expect(result.current.nextCursor).toBe('cur-page-2');
    expect(result.current.isLoadingMore).toBe(false);
  });

  it('loadMore appends new sessions and dedupes overlapping IDs', async () => {
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }, { session_id: 's2' }],
        next_cursor: 'cur-page-2',
      })
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's2' }, { session_id: 's3' }],
        next_cursor: 'cur-page-3',
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(2);
    });

    await act(async () => {
      await result.current.loadMore();
    });

    expect(api.listSessions).toHaveBeenLastCalledWith('agent-1', expect.objectContaining({
      limit: 50,
      cursor: 'cur-page-2',
    }));
    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2', 's3']);
    expect(result.current.nextCursor).toBe('cur-page-3');
    expect(result.current.hasMore).toBe(true);
  });

  it('next_cursor null stops further loading', async () => {
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }],
        next_cursor: 'cur-end',
      })
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's2' }],
        next_cursor: null,
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(1);
    });

    await act(async () => {
      await result.current.loadMore();
    });

    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2']);
    expect(result.current.hasMore).toBe(false);
    expect(result.current.nextCursor).toBeNull();

    // Calling loadMore again when hasMore is false does nothing
    await act(async () => {
      await result.current.loadMore();
    });
    expect(api.listSessions).toHaveBeenCalledTimes(2);
  });

  it('guards against same-tick double triggers of loadMore issuing only one request', async () => {
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }],
        next_cursor: 'cur-page-2',
      })
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's2' }],
        next_cursor: null,
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(1);
    });

    // Double trigger in the same tick
    await act(async () => {
      const p1 = result.current.loadMore();
      const p2 = result.current.loadMore();
      await Promise.all([p1, p2]);
    });

    // Initial page 1 fetch (1) + only ONE loadMore fetch (2)
    expect(api.listSessions).toHaveBeenCalledTimes(2);
    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2']);
  });

  it('reconcile does not duplicate or lose already-loaded older items', async () => {
    // Page 1 initial
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1', title: 'Session 1' }, { session_id: 's2', title: 'Session 2' }],
        next_cursor: 'cur-page-2',
      })
      // Page 2 load more
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's3', title: 'Session 3' }, { session_id: 's4', title: 'Session 4' }],
        next_cursor: null,
      })
      // Reconcile: page 1 returns a new session and updated s1
      .mockResolvedValueOnce({
        sessions: [
          { session_id: 's0', title: 'Newest Session' },
          { session_id: 's1', title: 'Session 1 Updated' },
        ],
        next_cursor: 'cur-page-2',
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(2);
    });

    // Load second page
    await act(async () => {
      await result.current.loadMore();
    });

    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2', 's3', 's4']);

    // Trigger reconcile
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      // s0 is added, s1 is updated, s2, s3, s4 are not lost, no duplicates
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s0', 's1', 's2', 's3', 's4']);
    });

    expect(result.current.sessions.find((s) => s.session_id === 's1')?.title).toBe('Session 1 Updated');
  });

  it('handles loadMore error and allows retry', async () => {
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }],
        next_cursor: 'cur-error',
      })
      .mockRejectedValueOnce(new Error('Network error loading page 2'))
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's2' }],
        next_cursor: null,
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(1);
    });

    // Attempt loadMore -> fails
    await act(async () => {
      await result.current.loadMore();
    });

    expect(result.current.loadMoreError).toBe('Network error loading page 2');
    expect(result.current.isLoadingMore).toBe(false);
    expect(result.current.sessions).toHaveLength(1);

    // Retry loadMore -> succeeds
    await act(async () => {
      await result.current.loadMore();
    });

    expect(result.current.loadMoreError).toBeNull();
    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2']);
  });

  it('preserves selected session even when not present in loaded pages', async () => {
    vi.mocked(api.listSessions).mockResolvedValueOnce({
      sessions: [{ session_id: 's1' }, { session_id: 's2' }],
      next_cursor: 'cur-page-2',
    });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: 'sess-deep-or-unloaded',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions.some((s) => s.session_id === 'sess-deep-or-unloaded')).toBe(true);
    });

    expect(result.current.sessions.map((s) => s.session_id)).toContain('sess-deep-or-unloaded');
  });

  it('prevents pagination lockout on deep-link with selectedSessionId not in first page', async () => {
    // Initial fetch on deep link
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }],
        next_cursor: 'cur-page-2',
      })
      // Reconcile
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }],
        next_cursor: 'cur-page-2',
      })
      // Load more
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's2' }],
        next_cursor: null,
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: 'sess-deep-not-in-p1',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 'sess-deep-not-in-p1']);
    });

    // next_cursor must not be locked out by placeholder
    expect(result.current.hasMore).toBe(true);
    expect(result.current.nextCursor).toBe('cur-page-2');

    // Trigger reconcile
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      expect(api.listSessions).toHaveBeenCalledTimes(2);
    });

    // hasMore must still be true after reconcile
    expect(result.current.hasMore).toBe(true);
    expect(result.current.nextCursor).toBe('cur-page-2');

    // User can load more
    await act(async () => {
      await result.current.loadMore();
    });

    expect(api.listSessions).toHaveBeenLastCalledWith('agent-1', expect.objectContaining({
      cursor: 'cur-page-2',
    }));
    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2', 'sess-deep-not-in-p1']);
    expect(result.current.hasMore).toBe(false);
  });

  it('prevents stale nextCursor after reconcile when only page 1 is loaded', async () => {
    // Initial fetch: page 1 returns s1, s2 with cursor pointing after s2
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }, { session_id: 's2' }],
        next_cursor: 'cur-old-tail',
      })
      // Reconcile: new session s0 pushed s2 out of page 1
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's0' }, { session_id: 's1' }],
        next_cursor: 'cur-new-tail',
      })
      // Load more
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's2' }, { session_id: 's3' }],
        next_cursor: null,
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2']);
    });
    expect(result.current.nextCursor).toBe('cur-old-tail');

    // Reconcile fires
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s0', 's1']);
    });

    // Cursor must be updated to new page 1 cursor, not kept as stale cur-old-tail
    expect(result.current.nextCursor).toBe('cur-new-tail');

    // loadMore uses cur-new-tail so it does not skip s2
    await act(async () => {
      await result.current.loadMore();
    });

    expect(api.listSessions).toHaveBeenLastCalledWith('agent-1', expect.objectContaining({
      cursor: 'cur-new-tail',
    }));
    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s0', 's1', 's2', 's3']);
  });

  it('prevents end-of-list lockout when reconcile receives non-null next cursor', async () => {
    // Initial fetch: all sessions fit in page 1, next_cursor is null
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1' }],
        next_cursor: null,
      })
      // Reconcile: new sessions added, page 1 now has next_cursor
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's0' }, { session_id: 's1' }],
        next_cursor: 'cur-has-more',
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1']);
    });
    expect(result.current.hasMore).toBe(false);
    expect(result.current.nextCursor).toBeNull();

    // Reconcile fires
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s0', 's1']);
    });

    // Must adopt non-null cursor instead of staying locked at null
    expect(result.current.hasMore).toBe(true);
    expect(result.current.nextCursor).toBe('cur-has-more');
  });

  it('guards against race when loadMore completes while reconcile is in flight', async () => {
    // Initial fetch: page 1
    vi.mocked(api.listSessions).mockResolvedValueOnce({
      sessions: [{ session_id: 's1' }],
      next_cursor: 'cur-p2',
    });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: '',
        setSelectedSessionId: vi.fn(),
      }),
    );

    await waitFor(() => {
      expect(result.current.sessions).toHaveLength(1);
    });

    // Create a deferred promise for reconcile so it stays in-flight
    let resolveReconcile!: (val: any) => void;
    const reconcilePromise = new Promise((resolve) => {
      resolveReconcile = resolve;
    });

    // Reconcile starts first
    vi.mocked(api.listSessions).mockImplementationOnce(() => reconcilePromise as any);
    act(() => {
      result.current.refreshSessions();
    });

    // While reconcile is in flight, loadMore runs and completes
    vi.mocked(api.listSessions).mockResolvedValueOnce({
      sessions: [{ session_id: 's2' }],
      next_cursor: 'cur-tail-advanced',
    });

    await act(async () => {
      await result.current.loadMore();
    });

    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 's2']);
    expect(result.current.nextCursor).toBe('cur-tail-advanced');

    // Now reconcile resolves with page 1 data
    await act(async () => {
      resolveReconcile({
        sessions: [{ session_id: 's0' }, { session_id: 's1' }],
        next_cursor: 'cur-p1-stale',
      });
    });

    // The cursor advanced by loadMore must NOT be clobbered by the older reconcile!
    expect(result.current.nextCursor).toBe('cur-tail-advanced');
    // s2 from loadMore must not be dropped
    expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s0', 's1', 's2']);
  });

  it('hydrates selected session placeholder with full metadata when fetched through loadMore', async () => {
    vi.mocked(api.listSessions)
      .mockResolvedValueOnce({
        sessions: [{ session_id: 's1', title: 'Session 1' }],
        next_cursor: 'cur-page-2',
      })
      .mockResolvedValueOnce({
        sessions: [
          {
            session_id: 'sess-selected',
            title: 'Full metadata session',
            repository: 'dobesv/harnx',
            branch: 'feat/meta',
            updated_at: '2026-01-01T00:00:00Z',
            unread: false,
          },
        ],
        next_cursor: null,
      });

    const { result } = renderHook(() =>
      useSessionDiscovery({
        selectedAgent: 'agent-1',
        selectedSessionId: 'sess-selected',
        setSelectedSessionId: vi.fn(),
      }),
    );

    // Initial page load has s1 and the unhydrated placeholder for sess-selected
    await waitFor(() => {
      expect(result.current.sessions.map((s) => s.session_id)).toEqual(['s1', 'sess-selected']);
    });

    const initialPlaceholder = result.current.sessions.find((s) => s.session_id === 'sess-selected');
    expect(initialPlaceholder?.title).toBeUndefined();

    // Fetch page 2 which contains the full metadata for sess-selected
    await act(async () => {
      await result.current.loadMore();
    });

    const hydratedSession = result.current.sessions.find((s) => s.session_id === 'sess-selected');
    expect(hydratedSession).toEqual({
      session_id: 'sess-selected',
      title: 'Full metadata session',
      repository: 'dobesv/harnx',
      branch: 'feat/meta',
      updated_at: '2026-01-01T00:00:00Z',
      unread: false,
    });
    expect(result.current.sessions).toHaveLength(2);
    expect(result.current.hasMore).toBe(false);
  });
});
