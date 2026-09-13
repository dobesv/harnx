import { useCallback, useEffect, useRef, useState } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { createSession, listSessions } from './api';
import type { SessionRef } from './types';
import { isAbortError } from './httpClient';

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

const SESSION_LIST_RECONCILE_INTERVAL_MS = 30000; // 30 seconds

function withoutListedSessions(ids: string[], sessions: SessionRef[]) {
  return ids.filter((id) => !sessions.some((session) => session.session_id === id));
}

function useSessionList(
  selectedAgent: string,
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>,
) {
  const [sessions, setSessions] = useState<SessionRef[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [requestLoading, setRequestLoading] = useState(false);
  const [settledAgent, setSettledAgent] = useState('');
  const [hasLoadedSessions, setHasLoadedSessions] = useState(false);

  const sessionsRequestRef = useRef(0);
  const abortControllerRef = useRef<AbortController | null>(null);
  const previousAgentRef = useRef(selectedAgent);

  // When selectedAgent changes to a different agent, reset immediately
  if (previousAgentRef.current !== selectedAgent) {
    previousAgentRef.current = selectedAgent;
    setSessions([]);
    setSessionsError(null);
    setSettledAgent('');
    setHasLoadedSessions(false);
  }

  const refreshSessions = useCallback(() => {
    // Abort previous in-flight request before starting a fresh attempt
    if (abortControllerRef.current) {
      abortControllerRef.current.abort();
      abortControllerRef.current = null;
    }

    const request = ++sessionsRequestRef.current;
    if (!selectedAgent) {
      setSessions([]);
      setSessionsError(null);
      setRequestLoading(false);
      setSettledAgent('');
      setHasLoadedSessions(false);
      return;
    }

    const controller = new AbortController();
    abortControllerRef.current = controller;

    setRequestLoading(true);
    setSessionsError(null);
    listSessions(selectedAgent, { signal: controller.signal })
      .then((data) => {
        if (request !== sessionsRequestRef.current) return;
        setSessions(data);
        setHasLoadedSessions(true);
        setFreshSessionIds((previous) => withoutListedSessions(previous, data));
      })
      .catch((error: unknown) => {
        if (request !== sessionsRequestRef.current) return;
        if (isAbortError(error)) return;
        console.error(error);
        setSessionsError(errorMessage(error, 'Failed to fetch sessions'));
      })
      .finally(() => {
        if (request !== sessionsRequestRef.current) return;
        setSettledAgent(selectedAgent);
        setRequestLoading(false);
      });
  }, [selectedAgent, setFreshSessionIds]);

  // Periodic reconcile: refetch on interval
  useEffect(() => {
    if (!selectedAgent) return;

    const interval = setInterval(() => {
      refreshSessions();
    }, SESSION_LIST_RECONCILE_INTERVAL_MS);

    return () => clearInterval(interval);
  }, [selectedAgent, refreshSessions]);

  useEffect(() => {
    refreshSessions();
    return () => {
      if (abortControllerRef.current) {
        abortControllerRef.current.abort();
        abortControllerRef.current = null;
      }
    };
  }, [refreshSessions]);

  return {
    sessions,
    setSessions,
    sessionsError,
    setSessionsError,
    sessionsLoading: Boolean(selectedAgent) && (requestLoading || settledAgent !== selectedAgent),
    hasLoadedSessions,
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

  const markSessionNotFresh = useCallback((sessionId: string) => {
    setFreshSessionIds((previous) => previous.filter((id) => id !== sessionId));
  }, []);

  const setSessionUnread = useCallback((sessionId: string, unread: boolean) => {
    sessionList.setSessions((prev) =>
      prev.map((s) => (s.session_id === sessionId ? { ...s, unread } : s)),
    );
  }, [sessionList.setSessions]);

  return {
    sessions: sessionList.sessions,
    sessionsError: sessionList.sessionsError,
    sessionsLoading: sessionList.sessionsLoading,
    hasLoadedSessions: sessionList.hasLoadedSessions,
    isFreshSession: freshSessionIds.includes(selectedSessionId),
    markSessionNotFresh,
    refreshSessions: sessionList.refreshSessions,
    setSessionUnread,
    selectSession,
    newChat,
  };
}
