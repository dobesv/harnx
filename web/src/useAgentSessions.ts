import { useState, useEffect, useCallback } from 'react';
import { listAgents, listSessions, newSessionId } from './api';
import type { Agent, SessionRef } from './types';

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
  const [sessions, setSessions] = useState<SessionRef[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string>(initial.agent);
  const [selectedSessionId, setSelectedSessionId] = useState<string>(initial.session);
  const [freshSessionIds, setFreshSessionIds] = useState<string[]>(() => (initial.session ? [] : []));

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

  const refreshSessions = useCallback(() => {
    if (!selectedAgent) return;
    setSessionsError(null);
    listSessions(selectedAgent).then(data => {
      setSessions(data);
      setFreshSessionIds((prev) => prev.filter((id) => !data.some((session) => session.session_id === id)));
    }).catch((err) => {
      console.error(err);
      setSessionsError(err.message || 'Failed to fetch sessions');
    });
  }, [selectedAgent]);

  useEffect(() => {
    setAgentsError(null);
    listAgents().then(data => {
      setAgents(data);
    }).catch((err) => {
      console.error(err);
      setAgentsError(err.message || 'Failed to fetch agents');
    });
  }, []);

  useEffect(() => {
    refreshSessions();
  }, [refreshSessions]);

  const handleNewChat = useCallback(() => {
    const id = newSessionId();
    setFreshSessionIds((prev) => [...prev, id]);
    setSelectedSessionId(id);
  }, []);

  const clearSession = useCallback(() => {
    setSelectedSessionId('');
  }, []);

  const clearAgent = useCallback(() => {
    setSelectedAgent('');
    setSelectedSessionId('');
  }, []);

  return {
    agents,
    agentsError,
    sessions,
    sessionsError,
    selectedAgent,
    selectedSessionId,
    isFreshSession: selectedSessionId ? freshSessionIds.includes(selectedSessionId) : false,
    refreshSessions,
    selectAgent: (agent: string) => {
      setSelectedAgent(agent);
      setSelectedSessionId('');
    },
    selectSession: (id: string) => {
      setSelectedSessionId(id);
      setFreshSessionIds((prev) => prev.filter((sessionId) => sessionId !== id));
    },
    newChat: handleNewChat,
    clearSession,
    clearAgent
  };
}