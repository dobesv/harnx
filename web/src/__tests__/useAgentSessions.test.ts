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

  it('fetches agents on mount', async () => {
    vi.mocked(api.listAgents).mockResolvedValue([{ name: 'agent1' } as any]);
    const { result } = renderHook(() => useAgentSessions());
    await waitFor(() => {
      expect(result.current.agents).toEqual([{ name: 'agent1' }]);
    });
  });

  it('fetches sessions when agent is selected', async () => {
    vi.mocked(api.listSessions).mockResolvedValue([{ session_id: 's1', updated_at: '2023-01-01' } as any]);
    const { result } = renderHook(() => useAgentSessions());
    
    act(() => {
      result.current.selectAgent('agent2');
    });

    await waitFor(() => {
      expect(result.current.selectedAgent).toBe('agent2');
      expect(result.current.sessions).toEqual([{ session_id: 's1', updated_at: '2023-01-01' }]);
    });
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
});
