import { useState, useEffect, useCallback } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { listAgents } from './api';
import type { Agent } from './types';
import { setDocumentTitle } from './sessionTitle';
import { useSessionDiscovery } from './useSessionDiscovery';
import { isAbortError } from './httpClient';

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
    const handlePopState = () => {
      const current = selectionFromLocation();
      setSelectedAgent((previous) => (previous === current.agent ? previous : current.agent));
      setSelectedSessionId((previous) => (previous === current.session ? previous : current.session));
    };

    window.addEventListener('popstate', handlePopState);
    return () => window.removeEventListener('popstate', handlePopState);
  }, [setSelectedAgent, setSelectedSessionId]);

  useEffect(() => {
    const current = selectionFromLocation();
    if (current.agent === selectedAgent && current.session === selectedSessionId) {
      return;
    }

    const nextPath = selectedAgent
      ? selectedSessionId
        ? `/agents/${encodeURIComponent(selectedAgent)}/sessions/${encodeURIComponent(selectedSessionId)}`
        : `/agents/${encodeURIComponent(selectedAgent)}`
      : '/';

    if (window.location.pathname !== nextPath) {
      window.history.pushState({}, '', nextPath);
    }
  }, [selectedAgent, selectedSessionId]);
}

export function useAgentSessions() {
  const initial = selectionFromLocation();

  const [agents, setAgents] = useState<Agent[]>([]);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const [hasLoadedAgents, setHasLoadedAgents] = useState(false);
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
    let active = true;
    const controller = new AbortController();
    setAgentsError(null);

    listAgents({ signal: controller.signal })
      .then((data) => {
        if (!active) return;
        setAgents(data);
        setHasLoadedAgents(true);
      })
      .catch((err: unknown) => {
        if (!active) return;
        if (isAbortError(err)) return;
        console.error(err);
        setAgentsError(
          err instanceof Error && err.message ? err.message : 'Failed to fetch agents'
        );
      });

    return () => {
      active = false;
      controller.abort();
    };
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
    hasLoadedAgents,
    sessions: discovery.sessions,
    sessionsError: discovery.sessionsError,
    sessionsLoading: discovery.sessionsLoading,
    hasLoadedSessions: discovery.hasLoadedSessions,
    selectedAgent,
    selectedSessionId,
    isFreshSession: discovery.isFreshSession,
    refreshSessions: discovery.refreshSessions,
    setSessionUnread: discovery.setSessionUnread,
    markSessionNotFresh: discovery.markSessionNotFresh,
    selectAgent,
    selectSession,
    navigateSession,
    newChat: discovery.newChat,
    clearSession,
    clearAgent
  };
}
