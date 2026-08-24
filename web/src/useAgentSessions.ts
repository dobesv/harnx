import { useState, useEffect, useCallback } from 'react';
import { listAgents } from './api';
import type { Agent } from './types';
import { setDocumentTitle } from './sessionTitle';
import { useSessionDiscovery } from './useSessionDiscovery';

export function useAgentSessions() {
  const getInitialState = () => {
    if (typeof window === 'undefined') return { agent: '', session: '' };
    const path = window.location.pathname;
    const sessionMatch = path.match(/^\/agents\/([^/]+)\/sessions\/([^/]+)/);
    const agentMatch = path.match(/^\/agents\/([^/]+)$/);
    if (sessionMatch) {
      return { agent: decodeURIComponent(sessionMatch[1]), session: decodeURIComponent(sessionMatch[2]) };
    } else if (agentMatch) {
      return { agent: decodeURIComponent(agentMatch[1]), session: '' };
    }
    return { agent: '', session: '' };
  };

  const initial = getInitialState();

  const [agents, setAgents] = useState<Agent[]>([]);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string>(initial.agent);
  const [selectedSessionId, setSelectedSessionId] = useState<string>(initial.session);
  const discovery = useSessionDiscovery({
    selectedAgent,
    selectedSessionId,
    setSelectedSessionId,
  });

  useEffect(() => {
    let newPath = '/';
    if (selectedAgent && selectedSessionId) {
      newPath = `/agents/${encodeURIComponent(selectedAgent)}/sessions/${encodeURIComponent(selectedSessionId)}`;
    } else if (selectedAgent) {
      newPath = `/agents/${encodeURIComponent(selectedAgent)}`;
    }

    const currentUrl = new URL(window.location.href);
    if (currentUrl.pathname !== newPath) {
      window.history.pushState(null, '', newPath + currentUrl.search);
    }
  }, [selectedAgent, selectedSessionId]);

  useEffect(() => {
    const onPopState = () => {
      const state = getInitialState();
      setSelectedAgent(state.agent);
      setSelectedSessionId(state.session);
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  useEffect(() => {
    setAgentsError(null);
    listAgents().then(data => {
      setAgents(data);
    }).catch((err) => {
      console.error(err);
      setAgentsError(err.message || 'Failed to fetch agents');
    });
  }, []);

  const clearSession = useCallback(() => {
    setSelectedSessionId('');
  }, []);

  const clearAgent = useCallback(() => {
    setSelectedAgent('');
    setSelectedSessionId('');
  }, []);


  useEffect(() => {
    const session = selectedSessionId
      ? discovery.sessions.find(s => s.session_id === selectedSessionId)
      : undefined;
    setDocumentTitle(session?.title);
  }, [selectedSessionId, discovery.sessions]);
  return {
    agents,
    agentsError,
    sessions: discovery.sessions,
    sessionsError: discovery.sessionsError,
    sessionsLoading: discovery.sessionsLoading,
    selectedAgent,
    selectedSessionId,
    isFreshSession: discovery.isFreshSession,
    refreshSessions: discovery.refreshSessions,
    selectAgent: (agent: string) => {
      setSelectedAgent(agent);
      setSelectedSessionId('');
    },
    selectSession: discovery.selectSession,
    newChat: discovery.newChat,
    clearSession,
    clearAgent
  };
}
