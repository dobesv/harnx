import { useState, useEffect, useCallback } from 'react';
import { listAgents, listSessions, newSessionId } from './api';
import type { Agent, SessionRef } from './types';

export function useAgentSessions() {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [sessions, setSessions] = useState<SessionRef[]>([]);
  const [selectedAgent, setSelectedAgent] = useState<string>('');
  const [selectedSessionId, setSelectedSessionId] = useState<string>('');

  const refreshSessions = useCallback(() => {
    if (!selectedAgent) return;
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
    }).catch(console.error);
  }, [selectedAgent]);

  useEffect(() => {
    listAgents().then(data => {
      setAgents(data);
      if (data.length > 0) {
        setSelectedAgent(data[0].name);
      }
    }).catch(console.error);
  }, []);

  useEffect(() => {
    refreshSessions();
  }, [refreshSessions]);

  const handleNewChat = useCallback(() => {
    setSelectedSessionId(newSessionId());
  }, []);

  return {
    agents,
    sessions,
    selectedAgent,
    selectedSessionId,
    refreshSessions,
    selectAgent: setSelectedAgent,
    selectSession: setSelectedSessionId,
    newChat: handleNewChat
  };
}
