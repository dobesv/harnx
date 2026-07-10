import { renderHook, act, waitFor } from '@testing-library/react';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { useAgentSessions } from '../useAgentSessions';
import * as api from '../api';

vi.mock('../api', () => ({
  listAgents: vi.fn(),
  listSessions: vi.fn(),
  newSessionId: vi.fn(),
}));

describe('useAgentSessions', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    window.history.pushState({}, '', '/');
    vi.mocked(api.listAgents).mockResolvedValue([]);
    vi.mocked(api.listSessions).mockResolvedValue([]);
    vi.mocked(api.newSessionId).mockReturnValue('new-session-uuid');
    
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
    
    act(() => {
      result.current.newChat(); // uses 'new-session-uuid'
    });
    
    expect(result.current.selectedSessionId).toBe('new-session-uuid');
    expect(result.current.isFreshSession).toBe(true);

    // Now backend returns it
    mockSessions = [{ session_id: 'new-session-uuid' }];
    act(() => {
      result.current.refreshSessions();
    });

    await waitFor(() => {
      expect(result.current.isFreshSession).toBe(false);
    });
  });
});
