import { render, screen, fireEvent, act } from '@testing-library/react';
import type { ComponentProps } from 'react';
import '@testing-library/jest-dom';
import { describe, it, expect, vi } from 'vitest';
import { SessionPicker } from '../App';

function renderSessionPicker(overrides: Partial<ComponentProps<typeof SessionPicker>> = {}) {
  const defaultProps: ComponentProps<typeof SessionPicker> = {
    agentName: 'test-agent',
    sessions: [{ session_id: 's1' }],
    sessionsError: null,
    sessionsLoading: false,
    hasLoadedSessions: true,
    onRetry: vi.fn(),
    onSelect: vi.fn(),
    onNewChat: vi.fn(),
    onBack: vi.fn(),
  };
  const props = { ...defaultProps, ...overrides };
  const utils = render(<SessionPicker {...props} />);
  return {
    ...utils,
    rerenderPicker: (nextOverrides: Partial<ComponentProps<typeof SessionPicker>> = {}) =>
      utils.rerender(<SessionPicker {...props} {...nextOverrides} />),
  };
}

describe('SessionPicker', () => {
  const sessions = [
    { session_id: 'session-old-read', updated_at: '2026-01-01T00:00:00Z', unread: false },
    { session_id: 'session-new-read', updated_at: '2026-01-03T00:00:00Z', unread: false },
    { session_id: 'session-old-unread', updated_at: '2026-01-02T00:00:00Z', unread: true },
    { session_id: 'session-new-unread', updated_at: '2026-01-04T00:00:00Z', unread: true },
  ];

  it('renders unread badges and sorts unread sessions first (then recency)', () => {
    renderSessionPicker({ sessions });

    const items = screen.getAllByRole('button').filter((b) => b.classList.contains('session-item'));
    expect(items).toHaveLength(4);
    // Order: session-new-unread, session-old-unread, session-new-read, session-old-read
    expect(items[0]).toHaveTextContent('session-new-unread');
    expect(items[1]).toHaveTextContent('session-old-unread');
    expect(items[2]).toHaveTextContent('session-new-read');
    expect(items[3]).toHaveTextContent('session-old-read');

    const badges = screen.getAllByTestId('session-unread-badge');
    expect(badges).toHaveLength(2);
    expect(badges[0]).toHaveTextContent('Unread');
    expect(badges[1]).toHaveTextContent('Unread');
  });

  it('toggles read/unread without triggering card selection', () => {
    const onSelect = vi.fn();
    const onToggleUnread = vi.fn();

    renderSessionPicker({
      sessions,
      onSelect,
      onToggleUnread,
    });

    // Find the toggle button on the first session (which is unread)
    const markReadBtn = screen.getByRole('button', { name: 'Mark session session-new-unread as read' });
    expect(markReadBtn).toHaveTextContent('Mark read');

    fireEvent.click(markReadBtn);
    expect(onToggleUnread).toHaveBeenCalledWith('session-new-unread', true);
    expect(onSelect).not.toHaveBeenCalled();

    // Find the toggle button on the third session (which is read)
    const markUnreadBtn = screen.getByRole('button', { name: 'Mark session session-new-read as unread' });
    expect(markUnreadBtn).toHaveTextContent('Mark unread');

    fireEvent.click(markUnreadBtn);
    expect(onToggleUnread).toHaveBeenCalledWith('session-new-read', false);
    expect(onSelect).not.toHaveBeenCalled();
  });

  it('renders title and repo/branch context on session cards when present', () => {
    const sessionsWithMeta = [
      {
        session_id: 'session-full',
        title: 'Full metadata session',
        repository: 'dobesv/harnx',
        branch: 'feat/meta',
        updated_at: '2026-01-01T00:00:00Z',
        unread: false,
      },
      {
        session_id: 'session-bare',
        title: null,
        repository: null,
        branch: null,
        updated_at: '2026-01-02T00:00:00Z',
        unread: false,
      },
    ];

    renderSessionPicker({ sessions: sessionsWithMeta });

    expect(screen.getByTestId('session-title')).toHaveTextContent('Full metadata session');
    expect(screen.getByTestId('session-context')).toHaveTextContent('dobesv/harnx @ feat/meta');
    expect(screen.getAllByTestId('session-title')).toHaveLength(1);
    expect(screen.getAllByTestId('session-context')).toHaveLength(1);
  });

  describe('incremental loading / load more control', () => {
    it('renders "Load more" button when hasMore is true and triggers onLoadMore on click', () => {
      const onLoadMore = vi.fn();
      renderSessionPicker({
        hasMore: true,
        isLoadingMore: false,
        onLoadMore,
      });

      const loadMoreBtn = screen.getByTestId('load-more-button');
      expect(loadMoreBtn).toBeInTheDocument();
      expect(loadMoreBtn).toHaveTextContent('Load more sessions');

      fireEvent.click(loadMoreBtn);
      expect(onLoadMore).toHaveBeenCalledTimes(1);
    });

    it('does not render "Load more" button when hasMore is false', () => {
      renderSessionPicker({ hasMore: false });

      expect(screen.queryByTestId('load-more-button')).not.toBeInTheDocument();
    });

    it('renders loading state with aria-busy and aria-disabled when isLoadingMore is true', () => {
      const onLoadMore = vi.fn();
      renderSessionPicker({
        hasMore: true,
        isLoadingMore: true,
        onLoadMore,
      });

      const btn = screen.getByTestId('load-more-button');
      expect(btn).toBeInTheDocument();
      expect(btn).toHaveAttribute('aria-disabled', 'true');
      expect(btn).toHaveAttribute('aria-busy', 'true');
      expect(btn).not.toHaveAttribute('disabled');
      expect(btn).toHaveTextContent('Loading more sessions…');
      expect(screen.getByRole('status')).toHaveTextContent('Loading more sessions…');

      // Clicking while loading must not call onLoadMore
      fireEvent.click(btn);
      expect(onLoadMore).not.toHaveBeenCalled();
    });

    it('renders error state and retry button when loadMoreError is set', () => {
      const onLoadMore = vi.fn();
      renderSessionPicker({
        hasMore: true,
        isLoadingMore: false,
        loadMoreError: 'Connection timeout',
        onLoadMore,
      });

      const errorAlert = screen.getByTestId('load-more-error');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent('Connection timeout');

      const retryBtn = screen.getByRole('button', { name: 'Retry' });
      fireEvent.click(retryBtn);
      expect(onLoadMore).toHaveBeenCalledTimes(1);
    });

    it('restores focus to load-more button when retrying clears an error across in-flight render', () => {
      const onLoadMore = vi.fn();
      const { rerenderPicker } = renderSessionPicker({
        hasMore: true,
        isLoadingMore: false,
        loadMoreError: 'Network failed',
        onLoadMore,
      });

      const retryBtn = screen.getByRole('button', { name: 'Retry' });
      fireEvent.click(retryBtn);
      expect(onLoadMore).toHaveBeenCalledTimes(1);

      // In-flight render while retry is running: error is cleared, isLoadingMore is true
      rerenderPicker({
        hasMore: true,
        isLoadingMore: true,
        loadMoreError: null,
      });

      // Retry finishes successfully: error is null, loading is false
      rerenderPicker({
        sessions: [{ session_id: 's1' }, { session_id: 's2' }],
        hasMore: true,
        isLoadingMore: false,
        loadMoreError: null,
      });

      const loadMoreBtn = screen.getByTestId('load-more-button');
      expect(document.activeElement).toBe(loadMoreBtn);
    });

    it('triggers onLoadMore via IntersectionObserver when sentinel intersects', () => {
      let observerCallback!: IntersectionObserverCallback;
      const observe = vi.fn();
      const disconnect = vi.fn();

      class MockIntersectionObserver implements IntersectionObserver {
        readonly root: Element | Document | null = null;
        readonly rootMargin: string = '';
        readonly scrollMargin: string = '';
        readonly thresholds: ReadonlyArray<number> = [];
        constructor(callback: IntersectionObserverCallback) {
          observerCallback = callback;
        }
        observe = observe;
        unobserve = vi.fn();
        disconnect = disconnect;
        takeRecords = () => [];
      }

      const originalIntersectionObserver = window.IntersectionObserver;
      window.IntersectionObserver = MockIntersectionObserver as any;

      const onLoadMore = vi.fn();
      try {
        renderSessionPicker({
          hasMore: true,
          isLoadingMore: false,
          onLoadMore,
        });

        expect(observe).toHaveBeenCalled();

        // Simulate intersection event
        act(() => {
          observerCallback(
            [{ isIntersecting: true } as IntersectionObserverEntry],
            {} as IntersectionObserver,
          );
        });

        expect(onLoadMore).toHaveBeenCalledTimes(1);
      } finally {
        window.IntersectionObserver = originalIntersectionObserver;
      }
    });

    it('does not re-trigger loadMore when observer replays isIntersecting:true after load completes', () => {
      let observerCallback!: IntersectionObserverCallback;
      const observe = vi.fn((_target: Element) => {
        // Mock observer that immediately replays initial intersection state on observe()
        observerCallback(
          [{ isIntersecting: true } as IntersectionObserverEntry],
          {} as IntersectionObserver,
        );
      });
      const disconnect = vi.fn();

      class MockIntersectionObserver implements IntersectionObserver {
        readonly root: Element | Document | null = null;
        readonly rootMargin: string = '';
        readonly scrollMargin: string = '';
        readonly thresholds: ReadonlyArray<number> = [];
        constructor(callback: IntersectionObserverCallback) {
          observerCallback = callback;
        }
        observe = observe;
        unobserve = vi.fn();
        disconnect = disconnect;
        takeRecords = () => [];
      }

      const originalIntersectionObserver = window.IntersectionObserver;
      window.IntersectionObserver = MockIntersectionObserver as any;

      const onLoadMore = vi.fn();
      try {
        const { rerenderPicker } = renderSessionPicker({
          hasMore: true,
          isLoadingMore: false,
          onLoadMore,
        });

        // Initial observation triggered one load
        expect(onLoadMore).toHaveBeenCalledTimes(1);

        // Simulate loadMore becoming in-flight: isLoadingMore = true
        rerenderPicker({
          hasMore: true,
          isLoadingMore: true,
        });

        // Simulate load completing: new session added, isLoadingMore = false
        rerenderPicker({
          sessions: [{ session_id: 's1' }, { session_id: 's2' }],
          hasMore: true,
          isLoadingMore: false,
        });

        // observe() was NOT called again (observer was not recreated on load completion)
        // and onLoadMore was NOT called a second time
        expect(observe).toHaveBeenCalledTimes(1);
        expect(onLoadMore).toHaveBeenCalledTimes(1);

        // Even if the observer callback fires again with isIntersecting: true while already intersecting:
        act(() => {
          observerCallback(
            [{ isIntersecting: true } as IntersectionObserverEntry],
            {} as IntersectionObserver,
          );
        });
        // Still only 1 call!
        expect(onLoadMore).toHaveBeenCalledTimes(1);

        // Only after user scrolls away (isIntersecting: false) and scrolls back (isIntersecting: true):
        act(() => {
          observerCallback(
            [{ isIntersecting: false } as IntersectionObserverEntry],
            {} as IntersectionObserver,
          );
        });
        expect(onLoadMore).toHaveBeenCalledTimes(1);

        act(() => {
          observerCallback(
            [{ isIntersecting: true } as IntersectionObserverEntry],
            {} as IntersectionObserver,
          );
        });
        expect(onLoadMore).toHaveBeenCalledTimes(2);
      } finally {
        window.IntersectionObserver = originalIntersectionObserver;
      }
    });
  });
});
