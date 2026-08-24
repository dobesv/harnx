import { useCallback, useEffect, useRef, useState } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { createSession, listSessions } from './api';
import type { SessionRef } from './types';

interface SessionDiscoveryOptions {
  selectedAgent: string;
  selectedSessionId: string;
  setSelectedSessionId: (sessionId: string) => void;
}

interface SessionCreationOptions {
  selectedAgent: string;
  setSelectedSessionId: (sessionId: string) => void;
  setSessionsError: Dispatch<SetStateAction<string | null>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
}

const errorMessage = (error: unknown, fallback: string) =>
  error instanceof Error && error.message ? error.message : fallback;

const withoutListedSessions = (ids: string[], sessions: SessionRef[]) =>
  ids.filter((id) => !sessions.some((session) => session.session_id === id));

function useSessionList(
  selectedAgent: string,
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>,
) {
  const [sessions, setSessions] = useState<SessionRef[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [requestLoading, setRequestLoading] = useState(false);
  const [settledAgent, setSettledAgent] = useState('');
  const sessionsRequestRef = useRef(0);

  const refreshSessions = useCallback(() => {
    const request = ++sessionsRequestRef.current;
    if (!selectedAgent) {
      setSessions([]);
      setSessionsError(null);
      setRequestLoading(false);
      setSettledAgent('');
      return;
    }

    setRequestLoading(true);
    setSessionsError(null);
    listSessions(selectedAgent)
      .then((data) => {
        if (request !== sessionsRequestRef.current) return;
        setSessions(data);
        setFreshSessionIds((previous) => withoutListedSessions(previous, data));
      })
      .catch((error: unknown) => {
        if (request !== sessionsRequestRef.current) return;
        console.error(error);
        setSessionsError(errorMessage(error, 'Failed to fetch sessions'));
      })
      .finally(() => {
        if (request !== sessionsRequestRef.current) return;
        setSettledAgent(selectedAgent);
        setRequestLoading(false);
      });
  }, [selectedAgent, setFreshSessionIds]);

  useEffect(refreshSessions, [refreshSessions]);

  return {
    sessions,
    sessionsError,
    setSessionsError,
    sessionsLoading: Boolean(selectedAgent) && (requestLoading || settledAgent !== selectedAgent),
    refreshSessions,
  };
}

function useSessionCreation({
  selectedAgent,
  setSelectedSessionId,
  setSessionsError,
  setFreshSessionIds,
}: SessionCreationOptions) {
  const selectedAgentRef = useRef(selectedAgent);
  const pendingAgentsRef = useRef(new Set<string>());
  selectedAgentRef.current = selectedAgent;

  return useCallback(async () => {
    const reservedAgent = selectedAgent;
    if (!reservedAgent || pendingAgentsRef.current.has(reservedAgent)) return;
    pendingAgentsRef.current.add(reservedAgent);
    setSessionsError(null);
    try {
      const session = await createSession(reservedAgent);
      if (selectedAgentRef.current !== reservedAgent) return;
      setFreshSessionIds((previous) => [...previous, session.session_id]);
      setSelectedSessionId(session.session_id);
    } catch (error: unknown) {
      if (selectedAgentRef.current !== reservedAgent) return;
      console.error(error);
      setSessionsError(errorMessage(error, 'Failed to create session'));
    } finally {
      pendingAgentsRef.current.delete(reservedAgent);
    }
  }, [selectedAgent, setFreshSessionIds, setSelectedSessionId, setSessionsError]);
}

export function useSessionDiscovery({
  selectedAgent,
  selectedSessionId,
  setSelectedSessionId,
}: SessionDiscoveryOptions) {
  const [freshSessionIds, setFreshSessionIds] = useState<string[]>([]);
  const sessionList = useSessionList(selectedAgent, setFreshSessionIds);
  const newChat = useSessionCreation({
    selectedAgent,
    setSelectedSessionId,
    setSessionsError: sessionList.setSessionsError,
    setFreshSessionIds,
  });

  const selectSession = useCallback((sessionId: string) => {
    setSelectedSessionId(sessionId);
    setFreshSessionIds((previous) => previous.filter((id) => id !== sessionId));
  }, [setSelectedSessionId]);

  return {
    sessions: sessionList.sessions,
    sessionsError: sessionList.sessionsError,
    sessionsLoading: sessionList.sessionsLoading,
    isFreshSession: freshSessionIds.includes(selectedSessionId),
    refreshSessions: sessionList.refreshSessions,
    selectSession,
    newChat,
  };
}
