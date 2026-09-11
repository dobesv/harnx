import { renderHook, act, waitFor } from '@testing-library/react';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { useAgentSessions } from '../useAgentSessions';
import * as api from '../api';

vi.mock('../api', () => ({
  listAgents: vi.fn(),
  listSessions: vi.fn(),
  createSession: vi.fn(),
}));

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

type SessionReservation = ReturnType<typeof deferred<{ session_id: string }>>;

async function expectStaleReservationIgnored(
  settle: (reservation: SessionReservation) => void,
  agents: [string, string],
) {
  const reservation = deferred<{ session_id: string }>();
  vi.mocked(api.createSession).mockReturnValue(reservation.promise);
  const { result, unmount } = renderHook(() => useAgentSessions());
  act(() => result.current.selectAgent(agents[0]));
  let request!: Promise<void>;
  act(() => {
    request = result.current.newChat();
    result.current.selectAgent(agents[1]);
  });

  settle(reservation);
  await act(async () => request);
  expect(result.current.selectedAgent).toBe(agents[1]);
  expect(result.current.selectedSessionId).toBe('');
  expect(result.current.isFreshSession).toBe(false);
  expect(result.current.sessionsError).toBeNull();
  unmount();
}

describe('useAgentSessions', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    window.history.pushState({}, '', '/');
    vi.mocked(api.listAgents).mockResolvedValue([]);
    vi.mocked(api.listSessions).mockResolvedValue([]);
    vi.mocked(api.createSession).mockResolvedValue({ session_id: 'new-session-id' });
    
    // Mute console.error for tests that expect errors
    vi.spyOn(console, 'error').mockImplementation(() => {});
  });

  it('parses empty url correctly', () => {
    const { result } = renderHook(() => useAgentSessions());
    expect(result.current.selectedAgent).toBe('');
    expect(result.current.selectedSessionId).toBe('');
    expect(result.current.hasLoadedAgents).toBe(false);
  });

  it('parses agent from url', () => {
    window.history.pushState({}, '', '/agents/test-agent');
    const { result } = renderHook(() => useAgentSessions());
    expect(result.current.selectedAgent).toBe('test-agent');
    expect(result.current.selectedSessionId).toBe('');
  });

  it('parses agent and session from url with special chars', () => {
    window.history.pushState({}, '', '/agents/my%2Fagent/sessions/sess%2B1');
    const { result } = renderHook(() => useAgentSessions());
    expect(result.current.selectedAgent).toBe('my/agent');
    expect(result.current.selectedSessionId).toBe('sess+1');
  });

  it('fetches agents on mount and tracks hasLoadedAgents', async () => {
    vi.mocked(api.listAgents).mockResolvedValue([{ name: 'agent1' } as any]);
    const { result } = renderHook(() => useAgentSessions());
    expect(result.current.hasLoadedAgents).toBe(false);
    await waitFor(() => {
      expect(result.current.agents).toEqual([{ name: 'agent1' }]);
      expect(result.current.hasLoadedAgents).toBe(true);
    });
  });

  it('fetches sessions when agent is selected and tracks hasLoadedSessions', async () => {
    vi.mocked(api.listSessions).mockResolvedValue([{ session_id: 's1', updated_at: '2023-01-01' } as any]);
    const { result } = renderHook(() => useAgentSessions());
    expect(result.current.hasLoadedSessions).toBe(false);
    
    act(() => {
      result.current.selectAgent('agent2');
    });

    await waitFor(() => {
      expect(result.current.selectedAgent).toBe('agent2');
      expect(result.current.sessions).toEqual([{ session_id: 's1', updated_at: '2023-01-01' }]);
      expect(result.current.hasLoadedSessions).toBe(true);
    });
  });

  it('resets sessions and hasLoadedSessions immediately when switching agents', async () => {
    vi.mocked(api.listSessions).mockResolvedValue([{ session_id: 's1' } as any]);
    const { result } = renderHook(() => useAgentSessions());

    act(() => {
      result.current.selectAgent('agent-a');
    });

    await waitFor(() => {
      expect(result.current.hasLoadedSessions).toBe(true);
      expect(result.current.sessions).toHaveLength(1);
    });

    // Switch to agent-b before mock returns
    const slowSessions = deferred<any[]>();
    vi.mocked(api.listSessions).mockReturnValue(slowSessions.promise);

    act(() => {
      result.current.selectAgent('agent-b');
    });

    // Should immediately reset to empty and not loaded
    expect(result.current.sessions).toEqual([]);
    expect(result.current.hasLoadedSessions).toBe(false);

    slowSessions.resolve([{ session_id: 's2' }]);
    await waitFor(() => {
      expect(result.current.hasLoadedSessions).toBe(true);
      expect(result.current.sessions).toEqual([{ session_id: 's2' }]);
    });
  });

  it('aborts listAgents on unmount', () => {
    const pending = deferred<any[]>();
    vi.mocked(api.listAgents).mockImplementation((options?: { signal?: AbortSignal }) => {
      options?.signal?.addEventListener('abort', () => {
        pending.reject(Object.assign(new Error('aborted'), { name: 'AbortError' }));
      });
      return pending.promise;
    });

    const { unmount } = renderHook(() => useAgentSessions());
    const abortSignal = vi.mocked(api.listAgents).mock.calls[0]?.[0]?.signal;
    expect(abortSignal?.aborted).toBe(false);

    unmount();
    expect(abortSignal?.aborted).toBe(true);
  });

  it('aborts listSessions on agent switch', async () => {
    const signals: AbortSignal[] = [];
    vi.mocked(api.listSessions).mockImplementation((_agent, options?: { signal?: AbortSignal }) => {
      if (options?.signal) signals.push(options.signal);
      return new Promise(() => {}); // never resolves
    });

    const { result } = renderHook(() => useAgentSessions());

    act(() => {
      result.current.selectAgent('agent-x');
    });

    expect(signals).toHaveLength(1);
    expect(signals[0].aborted).toBe(false);

    act(() => {
      result.current.selectAgent('agent-y');
    });

    expect(signals[0].aborted).toBe(true);
    expect(signals).toHaveLength(2);
    expect(signals[1].aborted).toBe(false);
  });

  it('keeps the session picker loading until discovery completes', async () => {
    let resolveSessions!: (sessions: any[]) => void;
    vi.mocked(api.listSessions).mockImplementation(() => new Promise((resolve) => {
      resolveSessions = resolve;
    }));
    const { result } = renderHook(() => useAgentSessions());

    act(() => result.current.selectAgent('slow-agent'));

    await waitFor(() => expect(result.current.sessionsLoading).toBe(true));
    expect(result.current.sessions).toEqual([]);

    await act(async () => resolveSessions([]));
    await waitFor(() => expect(result.current.sessionsLoading).toBe(false));
  });

  it('handles pushState/popstate sync', () => {
    const { result } = renderHook(() => useAgentSessions());
    
    act(() => {
      result.current.selectAgent('agent3');
      result.current.selectSession('sess3');
    });
    
    expect(window.location.pathname).toBe('/agents/agent3/sessions/sess3');

    act(() => {
      window.history.pushState({}, '', '/agents/agent4');
      window.dispatchEvent(new PopStateEvent('popstate'));
    });
    
    expect(result.current.selectedAgent).toBe('agent4');
    expect(result.current.selectedSessionId).toBe('');
  });

  it('navigates directly to another agent session with one history entry', () => {
    window.history.pushState({}, '', '/agents/parent/sessions/parent-session');
    const initialHistoryLength = window.history.length;
    const { result } = renderHook(() => useAgentSessions());

    act(() => result.current.navigateSession('pkg/child', 'child session'));

    expect(result.current.selectedAgent).toBe('pkg/child');
    expect(result.current.selectedSessionId).toBe('child session');
    expect(window.location.pathname).toBe('/agents/pkg%2Fchild/sessions/child%20session');
    expect(window.history.length).toBe(initialHistoryLength + 1);
  });

  it('freshSessionIds lifecycle: added on new chat, pruned when backend returns it', async () => {
    let mockSessions: any[] = [];
    vi.mocked(api.listSessions).mockImplementation(() => Promise.resolve(mockSessions));
    
    const { result } = renderHook(() => useAgentSessions());
    
    act(() => {
      result.current.selectAgent('agent5');
    });
    
    await act(async () => {
      await result.current.newChat();
    });
    
    expect(api.createSession).toHaveBeenCalledWith('agent5');
    expect(result.current.selectedSessionId).toBe('new-session-id');
    expect(result.current.isFreshSession).toBe(true);

    // Now backend returns it
    mockSessions = [{ session_id: 'new-session-id' }];
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      expect(result.current.isFreshSession).toBe(false);
    });
  });

  it('coalesces repeated new-chat actions while an agent reservation is pending', async () => {
    const reservation = deferred<{ session_id: string }>();
    vi.mocked(api.createSession).mockReturnValue(reservation.promise);
    const { result } = renderHook(() => useAgentSessions());
    act(() => result.current.selectAgent('agent6'));

    let first!: Promise<void>;
    act(() => {
      first = result.current.newChat();
      void result.current.newChat();
    });
    expect(api.createSession).toHaveBeenCalledTimes(1);

    reservation.resolve({ session_id: 'only-session' });
    await act(async () => first);
    expect(result.current.selectedSessionId).toBe('only-session');
  });

  it('ignores settled session reservations after switching agents', async () => {
    await expectStaleReservationIgnored(
      (reservation) => reservation.resolve({ session_id: 'stale-session' }),
      ['agent7', 'agent8'],
    );
    await expectStaleReservationIgnored(
      (reservation) => reservation.reject(new Error('stale failure')),
      ['agent9', 'agent10'],
    );
  });

  it('markSessionNotFresh transitions isFreshSession from true to false', async () => {
    vi.mocked(api.createSession).mockResolvedValueOnce({ session_id: 'test-fresh' });
    vi.mocked(api.listSessions).mockResolvedValue([]);

    const { result } = renderHook(() => useAgentSessions());
    
    act(() => {
      result.current.selectAgent('agent-fresh');
    });

    await act(async () => {
      await result.current.newChat();
    });

    expect(result.current.selectedSessionId).toBe('test-fresh');
    expect(result.current.isFreshSession).toBe(true);

    act(() => {
      result.current.markSessionNotFresh('test-fresh');
    });

    expect(result.current.isFreshSession).toBe(false);
  });
});
