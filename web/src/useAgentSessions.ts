import { useState, useEffect, useCallback } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { listAgents } from './api';
import type { Agent } from './types';
import { setDocumentTitle } from './sessionTitle';
import { useSessionDiscovery } from './useSessionDiscovery';

function selectionFromLocation() {
  if (typeof window === 'undefined') return { agent: '', session: '' };
  const path = window.location.pathname;
  const sessionMatch = path.match(/^\/agents\/([^/]+)\/sessions\/([^/]+)/);
  const agentMatch = path.match(/^\/agents\/([^/]+)$/);
  if (sessionMatch) {
    return {
      agent: decodeURIComponent(sessionMatch[1]),
      session: decodeURIComponent(sessionMatch[2]),
    };
  }
  if (agentMatch) {
    return { agent: decodeURIComponent(agentMatch[1]), session: '' };
  }
  return { agent: '', session: '' };
}

function useRouteSynchronization(
  selectedAgent: string,
  selectedSessionId: string,
  setSelectedAgent: Dispatch<SetStateAction<string>>,
  setSelectedSessionId: Dispatch<SetStateAction<string>>,
) {
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
      const state = selectionFromLocation();
      setSelectedAgent(state.agent);
      setSelectedSessionId(state.session);
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, [setSelectedAgent, setSelectedSessionId]);
}

export function useAgentSessions() {
  const initial = selectionFromLocation();

  const [agents, setAgents] = useState<Agent[]>([]);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string>(initial.agent);
  const [selectedSessionId, setSelectedSessionId] = useState<string>(initial.session);
  const discovery = useSessionDiscovery({
    selectedAgent,
    selectedSessionId,
    setSelectedSessionId,
  });
  const selectSession = discovery.selectSession;
  useRouteSynchronization(
    selectedAgent,
    selectedSessionId,
    setSelectedAgent,
    setSelectedSessionId,
  );

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

  const selectAgent = useCallback((agent: string) => {
    setSelectedAgent(agent);
    setSelectedSessionId('');
  }, []);

  const navigateSession = useCallback((agent: string, sessionId: string) => {
    setSelectedAgent(agent);
    selectSession(sessionId);
  }, [selectSession]);

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
    selectAgent,
    selectSession,
    navigateSession,
    newChat: discovery.newChat,
    clearSession,
    clearAgent
  };
}
