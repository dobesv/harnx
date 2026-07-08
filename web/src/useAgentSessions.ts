import { useState, useEffect, useCallback } from 'react';
import { listAgents, listSessions, newSessionId } from './api';
import type { Agent, SessionRef } from './types';

export function useAgentSessions() {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const [sessions, setSessions] = useState<SessionRef[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string>('');
  const [selectedSessionId, setSelectedSessionId] = useState<string>('');

  const refreshSessions = useCallback(() => {
    if (!selectedAgent) return;
    setSessionsError(null);
    listSessions(selectedAgent).then(data => {
      setSessions(data);
      setSelectedSessionId(prev => {
        if (data.length > 0 && !prev) {
          return data[0].session_id;
        } else if (data.length === 0) {
          return '';
        }
        return prev;
      });
    }).catch((err) => {
      console.error(err);
      setSessionsError(err.message || 'Failed to fetch sessions');
    });
  }, [selectedAgent]);

  useEffect(() => {
    setAgentsError(null);
    listAgents().then(data => {
      setAgents(data);
      if (data.length > 0) {
        setSelectedAgent(data[0].name);
      }
    }).catch((err) => {
      console.error(err);
      setAgentsError(err.message || 'Failed to fetch agents');
    });
  }, []);

  useEffect(() => {
    refreshSessions();
  }, [refreshSessions]);

  const handleNewChat = useCallback(() => {
    setSelectedSessionId(newSessionId());
  }, []);

  return {
    agents,
    agentsError,
    sessions,
    sessionsError,
    selectedAgent,
    selectedSessionId,
    refreshSessions,
    selectAgent: setSelectedAgent,
    selectSession: setSelectedSessionId,
    newChat: handleNewChat
  };
}
