import type { RefObject } from 'react';
import { useSessionPagination } from './useSessionPagination';

export interface SessionLoadMoreProps {
  containerRef: RefObject<HTMLElement | null>;
  hasMore?: boolean;
  isLoadingMore?: boolean;
  loadMoreError?: string | null;
  onLoadMore?: () => void;
}

export const SessionLoadMore = ({
  containerRef,
  hasMore,
  isLoadingMore,
  loadMoreError,
  onLoadMore,
}: SessionLoadMoreProps) => {
  const { sentinelRef, loadMoreBtnRef } = useSessionPagination({
    containerRef,
    hasMore,
    isLoadingMore,
    loadMoreError,
    onLoadMore,
  });

  const handleClick = isLoadingMore ? undefined : onLoadMore;

  if (loadMoreError) {
    return (
      <div className="session-load-more" ref={sentinelRef}>
        <div role="alert" className="aui-error load-more-error" data-testid="load-more-error">
          <span>{loadMoreError}</span>
          <button
            type="button"
            className="load-more-retry-btn"
            onClick={handleClick}
            aria-disabled={isLoadingMore}
            aria-busy={isLoadingMore}
          >
            Retry
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="session-load-more" ref={sentinelRef}>
      <button
        ref={loadMoreBtnRef}
        type="button"
        className="load-more-btn"
        data-testid="load-more-button"
        onClick={handleClick}
        aria-disabled={isLoadingMore}
        aria-busy={isLoadingMore}
      >
        {isLoadingMore ? 'Loading more sessions…' : 'Load more sessions'}
      </button>
      {isLoadingMore ? (
        <span className="aui-visually-hidden" role="status">
          Loading more sessions…
        </span>
      ) : null}
    </div>
  );
};
