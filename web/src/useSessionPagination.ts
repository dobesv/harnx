import { useEffect, useRef, type RefObject } from 'react';

export interface UseSessionPaginationOptions {
  containerRef: RefObject<HTMLElement | null>;
  hasMore?: boolean;
  isLoadingMore?: boolean;
  loadMoreError?: string | null;
  onLoadMore?: () => void;
}

export interface UseSessionPaginationResult {
  sentinelRef: RefObject<HTMLDivElement | null>;
  loadMoreBtnRef: RefObject<HTMLButtonElement | null>;
}

interface ObserverHandlerContext {
  onLoadMoreRef: RefObject<(() => void) | undefined>;
  hasMoreRef: RefObject<boolean | undefined>;
  isLoadingMoreRef: RefObject<boolean | undefined>;
  loadMoreErrorRef: RefObject<string | null | undefined>;
  wasIntersectingRef: RefObject<boolean>;
}

export interface SessionPaginationObserverOptions {
  sentinelRef: RefObject<HTMLDivElement | null>;
  containerRef: RefObject<HTMLElement | null>;
  hasMore?: boolean;
  isLoadingMore?: boolean;
  loadMoreError?: string | null;
  onLoadMore?: () => void;
}

function shouldTriggerAutoLoad(
  hasMore: boolean,
  isLoading: boolean,
  hasError: boolean,
  hasCallback: boolean,
): boolean {
  if (!hasMore || isLoading) {
    return false;
  }
  return !hasError && hasCallback;
}

function handleIntersectionEntry(
  entry: IntersectionObserverEntry | undefined,
  ctx: ObserverHandlerContext,
): void {
  if (!entry) {
    return;
  }

  const isIntersecting = entry.isIntersecting;
  const wasIntersecting = ctx.wasIntersectingRef.current;
  ctx.wasIntersectingRef.current = isIntersecting;

  if (!isIntersecting || wasIntersecting) {
    return;
  }

  const canLoad = shouldTriggerAutoLoad(
    Boolean(ctx.hasMoreRef.current),
    Boolean(ctx.isLoadingMoreRef.current),
    Boolean(ctx.loadMoreErrorRef.current),
    Boolean(ctx.onLoadMoreRef.current),
  );

  if (canLoad) {
    ctx.isLoadingMoreRef.current = true;
    ctx.onLoadMoreRef.current?.();
  }
}

/**
 * Restores focus to the load-more button when an in-flight retry clears a previous error.
 */
export function useLoadMoreFocus(
  buttonRef: RefObject<HTMLButtonElement | null>,
  hasError: boolean,
  isLoading: boolean,
): void {
  const hadErrorRef = useRef(false);

  useEffect(() => {
    if (hasError) {
      hadErrorRef.current = true;
      return;
    }
    if (isLoading) {
      return;
    }
    if (hadErrorRef.current) {
      hadErrorRef.current = false;
      buttonRef.current?.focus();
    }
  }, [hasError, isLoading, buttonRef]);
}

/**
 * Observes the sentinel element and triggers onLoadMore on intersection.
 * Prevents cascading requests using a transition gate (isIntersecting && !wasIntersecting).
 */
export function useSessionPaginationObserver({
  sentinelRef,
  containerRef,
  hasMore,
  isLoadingMore,
  loadMoreError,
  onLoadMore,
}: SessionPaginationObserverOptions): void {
  const onLoadMoreRef = useRef(onLoadMore);
  const hasMoreRef = useRef(hasMore);
  const isLoadingMoreRef = useRef(isLoadingMore);
  const loadMoreErrorRef = useRef(loadMoreError);
  const wasIntersectingRef = useRef(false);

  useEffect(() => {
    onLoadMoreRef.current = onLoadMore;
    hasMoreRef.current = hasMore;
    isLoadingMoreRef.current = isLoadingMore;
    loadMoreErrorRef.current = loadMoreError;
  });

  useEffect(() => {
    if (!hasMore || typeof IntersectionObserver === 'undefined') {
      return;
    }

    const sentinel = sentinelRef.current;
    const container = containerRef.current;
    if (!sentinel) {
      return;
    }

    const ctx: ObserverHandlerContext = {
      onLoadMoreRef,
      hasMoreRef,
      isLoadingMoreRef,
      loadMoreErrorRef,
      wasIntersectingRef,
    };

    const observer = new IntersectionObserver(
      (entries) => {
        handleIntersectionEntry(entries[0], ctx);
      },
      {
        root: container,
        rootMargin: '200px',
      },
    );

    observer.observe(sentinel);

    return () => {
      observer.disconnect();
      wasIntersectingRef.current = false;
    };
  }, [hasMore, containerRef, sentinelRef]);
}

export function useSessionPagination({
  containerRef,
  hasMore,
  isLoadingMore,
  loadMoreError,
  onLoadMore,
}: UseSessionPaginationOptions): UseSessionPaginationResult {
  const sentinelRef = useRef<HTMLDivElement | null>(null);
  const loadMoreBtnRef = useRef<HTMLButtonElement | null>(null);

  useLoadMoreFocus(loadMoreBtnRef, Boolean(loadMoreError), Boolean(isLoadingMore));
  useSessionPaginationObserver({
    sentinelRef,
    containerRef,
    hasMore,
    isLoadingMore,
    loadMoreError,
    onLoadMore,
  });

  return { sentinelRef, loadMoreBtnRef };
}
