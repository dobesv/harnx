/**
 * Session discovery hook with keyset pagination and periodic reconciliation.
 *
 * Key pagination invariants (see web/README.md):
 * - `hasLoadedExtraPagesRef` distinguishes single-page refresh from multi-page merge.
 * - `loadMoreVersionRef` guards against reconcile clobbering an advanced cursor.
 * - `selectedSessionId` placeholder is excluded from cursor/presence checks.
 * - `isLoadingMoreRef` is set synchronously before async tick to prevent duplicate triggers.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import type { Dispatch, MutableRefObject, SetStateAction } from 'react';
import { createSession, listSessions } from './api';
import type { PaginatedSessions, SessionRef } from './types';
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

interface SessionPaginationLoaderOptions {
  selectedAgent: string;
  selectedSessionIdRef: MutableRefObject<string | undefined>;
  previousAgentRef: MutableRefObject<string>;
  setSessions: Dispatch<SetStateAction<SessionRef[]>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
}

interface LoadMoreActionOptions {
  selectedAgent: string;
  nextCursorRef: MutableRefObject<string | null>;
  isLoadingMoreRef: MutableRefObject<boolean>;
  loadMoreAbortControllerRef: MutableRefObject<AbortController | null>;
  previousAgentRef: MutableRefObject<string>;
  selectedSessionIdRef: MutableRefObject<string | undefined>;
  hasLoadedExtraPagesRef: MutableRefObject<boolean>;
  loadMoreVersionRef: MutableRefObject<number>;
  setSessions: Dispatch<SetStateAction<SessionRef[]>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
  setIsLoadingMore: Dispatch<SetStateAction<boolean>>;
  setLoadMoreError: Dispatch<SetStateAction<string | null>>;
  adoptNextCursor: (cursor: string | null) => void;
  abortLoadMore: () => void;
}

interface SessionRefreshHookOptions {
  selectedAgent: string;
  selectedSessionIdRef: MutableRefObject<string | undefined>;
  loadMoreVersionRef: MutableRefObject<number>;
  hasLoadedExtraPagesRef: MutableRefObject<boolean>;
  adoptNextCursor: (cursor: string | null) => void;
  clearLoadMoreError: () => void;
  setSessions: Dispatch<SetStateAction<SessionRef[]>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
  setSessionsError: Dispatch<SetStateAction<string | null>>;
  setRequestLoading: Dispatch<SetStateAction<boolean>>;
  setSettledAgent: Dispatch<SetStateAction<string>>;
  setHasLoadedSessions: Dispatch<SetStateAction<boolean>>;
  resetForEmptyAgent: () => void;
}

interface LoadMoreExecutionParams {
  agent: string;
  cursor: string;
  signal: AbortSignal;
  previousAgentRef: MutableRefObject<string>;
  selectedSessionIdRef: MutableRefObject<string | undefined>;
  setSessions: Dispatch<SetStateAction<SessionRef[]>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
  adoptNextCursor: (cursor: string | null) => void;
  hasLoadedExtraPagesRef: MutableRefObject<boolean>;
  loadMoreVersionRef: MutableRefObject<number>;
}

interface ExecuteRefreshParams {
  agent: string;
  signal: AbortSignal;
  request: number;
  sessionsRequestRef: MutableRefObject<number>;
  selectedSessionIdRef: MutableRefObject<string | undefined>;
  loadMoreVersionRef: MutableRefObject<number>;
  loadMoreVersionAtStart: number;
  hasLoadedExtraPagesRef: MutableRefObject<boolean>;
  adoptNextCursor: (cursor: string | null) => void;
  setSessions: Dispatch<SetStateAction<SessionRef[]>>;
  setHasLoadedSessions: Dispatch<SetStateAction<boolean>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
}

interface AgentSwitchParams {
  currentAgent: string;
  previousAgentRef: MutableRefObject<string>;
  abortPrimaryRequest: () => void;
  resetPagination: () => void;
  resetListState: () => void;
}

const errorMessage = (error: unknown, fallback: string) =>
  error instanceof Error && error.message ? error.message : fallback;

const SESSION_LIST_RECONCILE_INTERVAL_MS = 30000; // 30 seconds
const SESSION_PAGE_SIZE = 50;

function withoutListedSessions(ids: string[], sessions: SessionRef[]) {
  return ids.filter((id) => !sessions.some((session) => session.session_id === id));
}

function normalizeSessionResponse(response: SessionRef[] | PaginatedSessions): {
  newSessions: SessionRef[];
  newNextCursor: string | null;
} {
  if (Array.isArray(response)) {
    return { newSessions: response, newNextCursor: null };
  }
  return {
    newSessions: response.sessions || [],
    newNextCursor: response.next_cursor ?? null,
  };
}

function mergeSessionsForPageOne(
  freshPage: SessionRef[],
  prevSessions: SessionRef[],
  selectedId?: string,
): SessionRef[] {
  const merged = [...freshPage];
  if (selectedId && !merged.some((s) => s.session_id === selectedId)) {
    const existingSelected = prevSessions.find((s) => s.session_id === selectedId);
    merged.push(existingSelected ?? { session_id: selectedId });
  }
  return merged;
}

function mergeSessionsForMultiPage(
  freshPage: SessionRef[],
  prevSessions: SessionRef[],
  selectedId?: string,
): SessionRef[] {
  const freshMap = new Map(freshPage.map((s) => [s.session_id, s]));
  const remainingOlder = prevSessions.filter((s) => !freshMap.has(s.session_id));
  const merged = [...freshPage, ...remainingOlder];

  if (selectedId && !merged.some((s) => s.session_id === selectedId)) {
    const existingSelected = prevSessions.find((s) => s.session_id === selectedId);
    merged.push(existingSelected ?? { session_id: selectedId });
  }

  return merged;
}

function appendPaginatedSessions(
  prev: SessionRef[],
  incoming: SessionRef[],
  selectedId?: string,
): SessionRef[] {
  const prevWithoutSelected = prev.filter((s) => s.session_id !== selectedId);
  const existingIds = new Set(prevWithoutSelected.map((s) => s.session_id));
  const uniqueIncoming = incoming.filter((s) => !existingIds.has(s.session_id));
  const combined = [...prevWithoutSelected, ...uniqueIncoming];

  if (selectedId && !combined.some((s) => s.session_id === selectedId)) {
    const existingSelected = prev.find((s) => s.session_id === selectedId);
    combined.push(existingSelected ?? { session_id: selectedId });
  }

  return combined;
}

function canLoadMoreSessions({
  agent,
  cursor,
  isLoading,
}: {
  agent: string;
  cursor: string | null;
  isLoading: boolean;
}): boolean {
  if (!agent) return false;
  if (!cursor) return false;
  return !isLoading;
}

function shouldAdoptPageOneCursor({
  extraPagesLoaded,
  loadMoreCompleted,
}: {
  extraPagesLoaded: boolean;
  loadMoreCompleted: boolean;
}): boolean {
  if (extraPagesLoaded) return false;
  return !loadMoreCompleted;
}

function mergeRefreshedSessions({
  freshSessions,
  prevSessions,
  selectedId,
  extraPagesLoaded,
  loadMoreCompletedDuringRefresh,
}: {
  freshSessions: SessionRef[];
  prevSessions: SessionRef[];
  selectedId?: string;
  extraPagesLoaded: boolean;
  loadMoreCompletedDuringRefresh: boolean;
}): SessionRef[] {
  if (!extraPagesLoaded && !loadMoreCompletedDuringRefresh) {
    return mergeSessionsForPageOne(freshSessions, prevSessions, selectedId);
  }
  return mergeSessionsForMultiPage(freshSessions, prevSessions, selectedId);
}

function handleRefreshSuccess({
  data,
  selectedId,
  extraPagesLoaded,
  loadMoreCompletedDuringRefresh,
  adoptNextCursor,
  setSessions,
  setHasLoadedSessions,
  setFreshSessionIds,
}: {
  data: SessionRef[] | PaginatedSessions;
  selectedId?: string;
  extraPagesLoaded: boolean;
  loadMoreCompletedDuringRefresh: boolean;
  adoptNextCursor: (cursor: string | null) => void;
  setSessions: Dispatch<SetStateAction<SessionRef[]>>;
  setHasLoadedSessions: Dispatch<SetStateAction<boolean>>;
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>;
}) {
  const { newSessions, newNextCursor } = normalizeSessionResponse(data);

  if (shouldAdoptPageOneCursor({ extraPagesLoaded, loadMoreCompleted: loadMoreCompletedDuringRefresh })) {
    adoptNextCursor(newNextCursor);
  }

  setSessions((prev) =>
    mergeRefreshedSessions({
      freshSessions: newSessions,
      prevSessions: prev,
      selectedId,
      extraPagesLoaded,
      loadMoreCompletedDuringRefresh,
    }),
  );

  setHasLoadedSessions(true);
  setFreshSessionIds((previous) => withoutListedSessions(previous, newSessions));
}

async function executeRefresh({
  agent,
  signal,
  request,
  sessionsRequestRef,
  selectedSessionIdRef,
  loadMoreVersionRef,
  loadMoreVersionAtStart,
  hasLoadedExtraPagesRef,
  adoptNextCursor,
  setSessions,
  setHasLoadedSessions,
  setFreshSessionIds,
}: ExecuteRefreshParams): Promise<void> {
  const data = await listSessions(agent, {
    limit: SESSION_PAGE_SIZE,
    signal,
  });

  if (request !== sessionsRequestRef.current) return;

  handleRefreshSuccess({
    data,
    selectedId: selectedSessionIdRef.current,
    extraPagesLoaded: hasLoadedExtraPagesRef.current,
    loadMoreCompletedDuringRefresh:
      loadMoreVersionRef.current !== loadMoreVersionAtStart,
    adoptNextCursor,
    setSessions,
    setHasLoadedSessions,
    setFreshSessionIds,
  });
}

function handleRefreshError({
  error,
  request,
  sessionsRequestRef,
  setSessionsError,
}: {
  error: unknown;
  request: number;
  sessionsRequestRef: MutableRefObject<number>;
  setSessionsError: Dispatch<SetStateAction<string | null>>;
}) {
  if (request !== sessionsRequestRef.current) return;
  if (isAbortError(error)) return;
  console.error(error);
  setSessionsError(errorMessage(error, 'Failed to fetch sessions'));
}

function finalizeRefresh({
  request,
  agent,
  sessionsRequestRef,
  setSettledAgent,
  setRequestLoading,
}: {
  request: number;
  agent: string;
  sessionsRequestRef: MutableRefObject<number>;
  setSettledAgent: Dispatch<SetStateAction<string>>;
  setRequestLoading: Dispatch<SetStateAction<boolean>>;
}) {
  if (request === sessionsRequestRef.current) {
    setSettledAgent(agent);
    setRequestLoading(false);
  }
}

function handleAgentSwitch({
  currentAgent,
  previousAgentRef,
  abortPrimaryRequest,
  resetPagination,
  resetListState,
}: AgentSwitchParams) {
  if (previousAgentRef.current !== currentAgent) {
    previousAgentRef.current = currentAgent;
    abortPrimaryRequest();
    resetPagination();
    resetListState();
  }
}

function includeSelectedSession(prev: SessionRef[], selectedId?: string): SessionRef[] {
  if (!selectedId) return prev;
  if (prev.some((s) => s.session_id === selectedId)) {
    return prev;
  }
  return [...prev, { session_id: selectedId }];
}

function isSessionListLoading({
  agent,
  requestLoading,
  settledAgent,
}: {
  agent: string;
  requestLoading: boolean;
  settledAgent: string;
}): boolean {
  if (!agent) return false;
  if (requestLoading) return true;
  return settledAgent !== agent;
}

function usePeriodicReconcile(callback: () => void, enabled: boolean) {
  useEffect(() => {
    if (!enabled) return;

    const interval = setInterval(() => {
      callback();
    }, SESSION_LIST_RECONCILE_INTERVAL_MS);

    return () => clearInterval(interval);
  }, [enabled, callback]);
}

async function executeLoadMore({
  agent,
  cursor,
  signal,
  previousAgentRef,
  selectedSessionIdRef,
  setSessions,
  setFreshSessionIds,
  adoptNextCursor,
  hasLoadedExtraPagesRef,
  loadMoreVersionRef,
}: LoadMoreExecutionParams): Promise<void> {
  const data = await listSessions(agent, {
    limit: SESSION_PAGE_SIZE,
    cursor,
    signal,
  });

  if (previousAgentRef.current !== agent) return;

  const { newSessions, newNextCursor } = normalizeSessionResponse(data);

  hasLoadedExtraPagesRef.current = true;
  loadMoreVersionRef.current += 1;

  adoptNextCursor(newNextCursor);

  setSessions((prev) =>
    appendPaginatedSessions(prev, newSessions, selectedSessionIdRef.current),
  );
  setFreshSessionIds((previous) => withoutListedSessions(previous, newSessions));
}

function handleLoadMoreError({
  error,
  agent,
  previousAgentRef,
  setLoadMoreError,
}: {
  error: unknown;
  agent: string;
  previousAgentRef: MutableRefObject<string>;
  setLoadMoreError: Dispatch<SetStateAction<string | null>>;
}) {
  if (previousAgentRef.current !== agent) return;
  if (isAbortError(error)) return;
  console.error(error);
  setLoadMoreError(errorMessage(error, 'Failed to load more sessions'));
}

function finalizeLoadMore({
  agent,
  previousAgentRef,
  isLoadingMoreRef,
  setIsLoadingMore,
}: {
  agent: string;
  previousAgentRef: MutableRefObject<string>;
  isLoadingMoreRef: MutableRefObject<boolean>;
  setIsLoadingMore: Dispatch<SetStateAction<boolean>>;
}) {
  if (previousAgentRef.current === agent) {
    isLoadingMoreRef.current = false;
    setIsLoadingMore(false);
  }
}

function useLoadMoreAction({
  selectedAgent,
  nextCursorRef,
  isLoadingMoreRef,
  loadMoreAbortControllerRef,
  previousAgentRef,
  selectedSessionIdRef,
  hasLoadedExtraPagesRef,
  loadMoreVersionRef,
  setSessions,
  setFreshSessionIds,
  setIsLoadingMore,
  setLoadMoreError,
  adoptNextCursor,
  abortLoadMore,
}: LoadMoreActionOptions) {
  return useCallback(async () => {
    const cursor = nextCursorRef.current;
    if (!canLoadMoreSessions({ agent: selectedAgent, cursor, isLoading: isLoadingMoreRef.current })) {
      return;
    }

    isLoadingMoreRef.current = true;
    abortLoadMore();

    const controller = new AbortController();
    loadMoreAbortControllerRef.current = controller;
    setIsLoadingMore(true);
    setLoadMoreError(null);

    const requestAgent = selectedAgent;

    try {
      await executeLoadMore({
        agent: requestAgent,
        cursor: cursor!,
        signal: controller.signal,
        previousAgentRef,
        selectedSessionIdRef,
        setSessions,
        setFreshSessionIds,
        adoptNextCursor,
        hasLoadedExtraPagesRef,
        loadMoreVersionRef,
      });
    } catch (error: unknown) {
      handleLoadMoreError({
        error,
        agent: requestAgent,
        previousAgentRef,
        setLoadMoreError,
      });
    } finally {
      finalizeLoadMore({
        agent: requestAgent,
        previousAgentRef,
        isLoadingMoreRef,
        setIsLoadingMore,
      });
    }
  }, [
    selectedAgent, adoptNextCursor, abortLoadMore, hasLoadedExtraPagesRef,
    isLoadingMoreRef, loadMoreAbortControllerRef, loadMoreVersionRef, nextCursorRef,
    previousAgentRef, selectedSessionIdRef, setFreshSessionIds, setIsLoadingMore,
    setLoadMoreError, setSessions,
  ]);
}

function useSessionPaginationLoader({
  selectedAgent,
  selectedSessionIdRef,
  previousAgentRef,
  setSessions,
  setFreshSessionIds,
}: SessionPaginationLoaderOptions) {
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [isLoadingMore, setIsLoadingMore] = useState(false);
  const [loadMoreError, setLoadMoreError] = useState<string | null>(null);

  const nextCursorRef = useRef<string | null>(null);
  nextCursorRef.current = nextCursor;
  const isLoadingMoreRef = useRef(isLoadingMore);
  isLoadingMoreRef.current = isLoadingMore;

  const hasLoadedExtraPagesRef = useRef(false);
  const loadMoreVersionRef = useRef(0);
  const loadMoreAbortControllerRef = useRef<AbortController | null>(null);

  const abortLoadMore = useCallback(() => {
    if (loadMoreAbortControllerRef.current) {
      loadMoreAbortControllerRef.current.abort();
      loadMoreAbortControllerRef.current = null;
    }
  }, []);

  const resetPagination = useCallback(() => {
    abortLoadMore();
    setNextCursor(null);
    nextCursorRef.current = null;
    setIsLoadingMore(false);
    isLoadingMoreRef.current = false;
    setLoadMoreError(null);
    hasLoadedExtraPagesRef.current = false;
    loadMoreVersionRef.current = 0;
  }, [abortLoadMore]);

  const adoptNextCursor = useCallback((cursor: string | null) => {
    setNextCursor(cursor);
    nextCursorRef.current = cursor;
  }, []);

  const clearLoadMoreError = useCallback(() => {
    setLoadMoreError(null);
  }, []);

  const loadMore = useLoadMoreAction({
    selectedAgent,
    nextCursorRef,
    isLoadingMoreRef,
    loadMoreAbortControllerRef,
    previousAgentRef,
    selectedSessionIdRef,
    hasLoadedExtraPagesRef,
    loadMoreVersionRef,
    setSessions,
    setFreshSessionIds,
    setIsLoadingMore,
    setLoadMoreError,
    adoptNextCursor,
    abortLoadMore,
  });

  return {
    nextCursor,
    isLoadingMore,
    loadMoreError,
    hasLoadedExtraPagesRef,
    loadMoreVersionRef,
    abortLoadMore,
    resetPagination,
    adoptNextCursor,
    clearLoadMoreError,
    loadMore,
  };
}

function useSessionRefresh({
  selectedAgent,
  selectedSessionIdRef,
  loadMoreVersionRef,
  hasLoadedExtraPagesRef,
  adoptNextCursor,
  clearLoadMoreError,
  setSessions,
  setFreshSessionIds,
  setSessionsError,
  setRequestLoading,
  setSettledAgent,
  setHasLoadedSessions,
  resetForEmptyAgent,
}: SessionRefreshHookOptions) {
  const sessionsRequestRef = useRef(0);
  const abortControllerRef = useRef<AbortController | null>(null);

  const abortPrimaryRequest = useCallback(() => {
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
  }, []);

  const refreshSessions = useCallback(async () => {
    abortPrimaryRequest();

    const request = ++sessionsRequestRef.current;
    if (!selectedAgent) {
      resetForEmptyAgent();
      return;
    }

    const controller = new AbortController();
    abortControllerRef.current = controller;
    const loadMoreVersionAtStart = loadMoreVersionRef.current;

    setRequestLoading(true);
    setSessionsError(null);
    clearLoadMoreError();

    try {
      await executeRefresh({
        agent: selectedAgent,
        signal: controller.signal,
        request,
        sessionsRequestRef,
        selectedSessionIdRef,
        loadMoreVersionRef,
        loadMoreVersionAtStart,
        hasLoadedExtraPagesRef,
        adoptNextCursor,
        setSessions,
        setHasLoadedSessions,
        setFreshSessionIds,
      });
    } catch (error: unknown) {
      handleRefreshError({ error, request, sessionsRequestRef, setSessionsError });
    } finally {
      finalizeRefresh({ request, agent: selectedAgent, sessionsRequestRef, setSettledAgent, setRequestLoading });
    }
  }, [
    selectedAgent, abortPrimaryRequest, resetForEmptyAgent, clearLoadMoreError,
    adoptNextCursor, setSessions, setHasLoadedSessions, setFreshSessionIds,
    setRequestLoading, setSessionsError, setSettledAgent,
    hasLoadedExtraPagesRef, loadMoreVersionRef, selectedSessionIdRef,
  ]);

  return {
    abortPrimaryRequest,
    refreshSessions,
  };
}

function useSessionList(
  selectedAgent: string,
  setFreshSessionIds: Dispatch<SetStateAction<string[]>>,
  selectedSessionId?: string,
) {
  const [sessions, setSessions] = useState<SessionRef[]>([]);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [requestLoading, setRequestLoading] = useState(false);
  const [settledAgent, setSettledAgent] = useState('');
  const [hasLoadedSessions, setHasLoadedSessions] = useState(false);

  const previousAgentRef = useRef(selectedAgent);
  const selectedSessionIdRef = useRef(selectedSessionId);
  selectedSessionIdRef.current = selectedSessionId;

  const pagination = useSessionPaginationLoader({
    selectedAgent, selectedSessionIdRef, previousAgentRef,
    setSessions, setFreshSessionIds,
  });

  const resetListState = useCallback(() => {
    setSessions([]);
    setSessionsError(null);
    setRequestLoading(false);
    setSettledAgent('');
    setHasLoadedSessions(false);
  }, []);

  const { resetPagination, adoptNextCursor, clearLoadMoreError, abortLoadMore } = pagination;

  const resetForEmptyAgent = useCallback(() => {
    resetListState();
    resetPagination();
  }, [resetListState, resetPagination]);

  const { abortPrimaryRequest, refreshSessions } = useSessionRefresh({
    selectedAgent, selectedSessionIdRef,
    loadMoreVersionRef: pagination.loadMoreVersionRef,
    hasLoadedExtraPagesRef: pagination.hasLoadedExtraPagesRef,
    adoptNextCursor, clearLoadMoreError,
    setSessions, setFreshSessionIds, setSessionsError,
    setRequestLoading, setSettledAgent, setHasLoadedSessions,
    resetForEmptyAgent,
  });

  handleAgentSwitch({
    currentAgent: selectedAgent, previousAgentRef,
    abortPrimaryRequest, resetPagination, resetListState,
  });

  useEffect(() => {
    setSessions((prev) => includeSelectedSession(prev, selectedSessionId));
  }, [selectedSessionId]);

  usePeriodicReconcile(refreshSessions, Boolean(selectedAgent));

  useEffect(() => {
    refreshSessions();
    return () => {
      abortPrimaryRequest();
      abortLoadMore();
    };
  }, [refreshSessions, abortPrimaryRequest, abortLoadMore]);

  return {
    sessions,
    setSessions,
    sessionsError,
    setSessionsError,
    sessionsLoading: isSessionListLoading({ agent: selectedAgent, requestLoading, settledAgent }),
    hasLoadedSessions,
    hasMore: Boolean(pagination.nextCursor),
    nextCursor: pagination.nextCursor,
    isLoadingMore: pagination.isLoadingMore,
    loadMoreError: pagination.loadMoreError,
    loadMore: pagination.loadMore,
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
  const sessionList = useSessionList(selectedAgent, setFreshSessionIds, selectedSessionId);
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

  const { setSessions } = sessionList;
  const setSessionUnread = useCallback((sessionId: string, unread: boolean) => {
    setSessions((prev) =>
      prev.map((s) => (s.session_id === sessionId ? { ...s, unread } : s)),
    );
  }, [setSessions]);

  return {
    sessions: sessionList.sessions,
    sessionsError: sessionList.sessionsError,
    sessionsLoading: sessionList.sessionsLoading,
    hasLoadedSessions: sessionList.hasLoadedSessions,
    hasMore: sessionList.hasMore,
    nextCursor: sessionList.nextCursor,
    isLoadingMore: sessionList.isLoadingMore,
    loadMoreError: sessionList.loadMoreError,
    loadMore: sessionList.loadMore,
    isFreshSession: freshSessionIds.includes(selectedSessionId),
    markSessionNotFresh,
    refreshSessions: sessionList.refreshSessions,
    setSessionUnread,
    selectSession,
    newChat,
  };
}
