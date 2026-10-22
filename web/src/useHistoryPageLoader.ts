import { useCallback, useEffect, useState, type RefObject } from 'react';
import { getSessionHistory } from './api';
import type { SessionHistoryParams, SessionHistoryResult } from './types';
import type { ArchivedHistoryTarget } from './ArchivedHistoryContext';

export type WindowLoadState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; data: SessionHistoryResult };

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return 'Unknown error loading archived history';
}

function buildHistoryParams(target: ArchivedHistoryTarget, cursor: string | null): SessionHistoryParams {
  if (cursor) return { cursor };
  return { before_compaction: target.beforeCompaction };
}

export function useHistoryPageLoader(
  target: ArchivedHistoryTarget,
  activeRequestIdRef: RefObject<number>,
) {
  const [loadState, setLoadState] = useState<WindowLoadState>({ status: 'loading' });
  const [activeCursor, setActiveCursor] = useState<string | null>(null);

  const executeFetch = useCallback(
    (targetToLoad: ArchivedHistoryTarget, cursor: string | null, signal?: AbortSignal) => {
      const requestId = ++activeRequestIdRef.current;
      setLoadState({ status: 'loading' });
      const params = buildHistoryParams(targetToLoad, cursor);

      getSessionHistory(targetToLoad.agent, targetToLoad.session, params, { signal })
        .then((data) => {
          if (requestId === activeRequestIdRef.current) {
            setLoadState({ status: 'loaded', data });
          }
        })
        .catch((err: unknown) => {
          if (signal?.aborted) return;
          if (requestId === activeRequestIdRef.current) {
            setLoadState({ status: 'error', message: errorMessage(err) });
          }
        });
    },
    [activeRequestIdRef],
  );

  useEffect(() => {
    const controller = new AbortController();
    executeFetch(target, activeCursor, controller.signal);
    return () => {
      controller.abort();
    };
  }, [target, activeCursor, executeFetch]);

  const handleRetry = useCallback(() => {
    executeFetch(target, activeCursor);
  }, [target, activeCursor, executeFetch]);

  const handleNextPage = useCallback((cursor: string) => {
    setActiveCursor(cursor);
  }, []);

  const handlePrevPage = useCallback((cursor: string) => {
    setActiveCursor(cursor);
  }, []);

  const resetCursor = useCallback(() => {
    setActiveCursor(null);
  }, []);

  return {
    loadState,
    handleRetry,
    handleNextPage,
    handlePrevPage,
    resetCursor,
  };
}
