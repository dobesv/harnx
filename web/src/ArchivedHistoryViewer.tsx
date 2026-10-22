import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ComponentPropsWithoutRef,
  type MouseEvent,
  type RefObject,
} from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import { PrismAsyncLight as SyntaxHighlighter } from 'react-syntax-highlighter';
import remarkGfm from 'remark-gfm';
import { getSessionHistory } from './api';
import type { HistoryMessage, SessionHistoryResult } from './types';
import type { ArchivedHistoryTarget } from './ArchivedHistoryContext';
import { MarkdownLink } from './markdownLink';
import { markdownUrlTransform } from './markdownUrl';

export interface ArchivedHistoryViewerProps {
  initialTarget: ArchivedHistoryTarget;
  onClose: () => void;
}

type WindowLoadState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; data: SessionHistoryResult };

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return 'Unknown error loading archived history';
}

export const FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function trapFocusDirection(
  event: KeyboardEvent,
  dialog: HTMLElement,
  firstElement: HTMLElement,
  lastElement: HTMLElement,
): void {
  const currentActive = globalThis.document.activeElement;
  if (event.shiftKey) {
    if (currentActive === firstElement || !dialog.contains(currentActive)) {
      event.preventDefault();
      lastElement.focus();
    }
  } else {
    if (currentActive === lastElement || !dialog.contains(currentActive)) {
      event.preventDefault();
      firstElement.focus();
    }
  }
}

function handleDialogTab(event: KeyboardEvent, dialog: HTMLElement | null): void {
  if (!dialog) return;
  const focusables = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR));
  if (focusables.length === 0) {
    event.preventDefault();
    return;
  }
  trapFocusDirection(event, dialog, focusables[0], focusables[focusables.length - 1]);
}

export function useHistoryDialog(
  dialogRef: RefObject<HTMLElement | null>,
  closeButtonRef: RefObject<HTMLButtonElement | null>,
  onClose: () => void,
): void {
  // Focus initial and restore on close
  useEffect(() => {
    const previouslyFocused = globalThis.document.activeElement;
    closeButtonRef.current?.focus();
    return () => {
      if (previouslyFocused instanceof HTMLElement) previouslyFocused.focus();
    };
  }, [closeButtonRef]);

  // Trap focus and handle Escape
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        onClose();
        return;
      }
      if (event.key === 'Tab') {
        handleDialogTab(event, dialogRef.current);
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [dialogRef, onClose]);

  // Inert background (.app-container) and lock body scroll
  useEffect(() => {
    const appContainer = globalThis.document.querySelector<HTMLElement>('.app-container');
    const prevInert = appContainer?.getAttribute('inert');
    const prevAriaHidden = appContainer?.getAttribute('aria-hidden');
    const prevBodyOverflow = globalThis.document.body.style.overflow;

    if (appContainer) {
      appContainer.setAttribute('inert', '');
      appContainer.setAttribute('aria-hidden', 'true');
    }
    globalThis.document.body.style.overflow = 'hidden';

    return () => {
      if (appContainer) {
        if (prevInert !== null && prevInert !== undefined) {
          appContainer.setAttribute('inert', prevInert);
        } else {
          appContainer.removeAttribute('inert');
        }
        if (prevAriaHidden !== null && prevAriaHidden !== undefined) {
          appContainer.setAttribute('aria-hidden', prevAriaHidden);
        } else {
          appContainer.removeAttribute('aria-hidden');
        }
      }
      globalThis.document.body.style.overflow = prevBodyOverflow;
    };
  }, []);
}

export function MarkdownCode({
  node: _node,
  className,
  children,
  ...props
}: ComponentPropsWithoutRef<'code'> & { node?: unknown }) {
  const language = /language-([\w-]+)/.exec(className || '')?.[1];
  if (language) {
    return (
      <SyntaxHighlighter
        language={language}
        PreTag="div"
        useInlineStyles={false}
        className="cid-document-code-block"
      >
        {String(children).replace(/\n$/, '')}
      </SyntaxHighlighter>
    );
  }

  return <code className={className} {...props}>{children}</code>;
}

function isSameTarget(a: ArchivedHistoryTarget, b: ArchivedHistoryTarget): boolean {
  return a.agent === b.agent && a.session === b.session && a.beforeCompaction === b.beforeCompaction;
}

function useHistoryNavigation(initialTarget: ArchivedHistoryTarget) {
  const [targetStack, setTargetStack] = useState<ArchivedHistoryTarget[]>([initialTarget]);
  const currentTarget = targetStack[targetStack.length - 1] ?? initialTarget;
  const canGoBack = targetStack.length > 1;

  useEffect(() => {
    setTargetStack((prev) => {
      const top = prev[0];
      if (top && isSameTarget(top, initialTarget)) return prev;
      return [initialTarget];
    });
  }, [initialTarget]);

  const goBack = useCallback(() => {
    if (targetStack.length <= 1) return;
    setTargetStack((prev) => prev.slice(0, -1));
  }, [targetStack.length]);

  const pushTarget = useCallback((compactionSeq: number) => {
    setTargetStack((prev) => {
      const last = prev[prev.length - 1];
      const agent = last?.agent ?? initialTarget.agent;
      const session = last?.session ?? initialTarget.session;
      return [
        ...prev,
        {
          agent,
          session,
          beforeCompaction: compactionSeq,
          title: `History before compaction ${compactionSeq}`,
        },
      ];
    });
  }, [initialTarget.agent, initialTarget.session]);

  return { currentTarget, canGoBack, goBack, pushTarget };
}

function useHistoryPageLoader(
  target: ArchivedHistoryTarget,
  activeRequestIdRef: RefObject<number>,
) {
  const [loadState, setLoadState] = useState<WindowLoadState>({ status: 'loading' });
  const [activeCursor, setActiveCursor] = useState<string | null>(null);

  const loadPage = useCallback(
    (targetToLoad: ArchivedHistoryTarget, cursor: string | null, signal?: AbortSignal) => {
      const requestId = ++activeRequestIdRef.current;
      setLoadState({ status: 'loading' });

      const params = cursor
        ? { cursor }
        : { before_compaction: targetToLoad.beforeCompaction };

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
    loadPage(target, activeCursor, controller.signal);
    return () => {
      controller.abort();
    };
  }, [target, activeCursor, loadPage]);

  const handleRetry = useCallback(() => {
    loadPage(target, activeCursor);
  }, [target, activeCursor, loadPage]);

  const handleNextPage = useCallback(() => {
    if (loadState.status === 'loaded' && loadState.data.next_cursor) {
      setActiveCursor(loadState.data.next_cursor);
    }
  }, [loadState]);

  const handlePrevPage = useCallback(() => {
    if (loadState.status === 'loaded' && loadState.data.prev_cursor) {
      setActiveCursor(loadState.data.prev_cursor);
    }
  }, [loadState]);

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

interface ViewerHeaderProps {
  title: string;
  agent: string;
  session: string;
  canGoBack: boolean;
  closeButtonRef: RefObject<HTMLButtonElement | null>;
  onBack: () => void;
  onClose: () => void;
}

function ViewerHeader({
  title,
  agent,
  session,
  canGoBack,
  closeButtonRef,
  onBack,
  onClose,
}: ViewerHeaderProps) {
  return (
    <header className="archived-history-header">
      <button
        type="button"
        className="archived-history-back"
        onClick={onBack}
        disabled={!canGoBack}
        aria-label="Back to more recent archived history"
        title="Back"
      >
        ←
      </button>
      <div className="archived-history-heading">
        <h2 id="archived-history-title">{title}</h2>
        <span className="archived-history-meta">
          Archived read-only transcript • {agent}/{session}
        </span>
      </div>
      <button
        ref={closeButtonRef}
        type="button"
        className="archived-history-close"
        onClick={onClose}
        aria-label="Close archived history viewer"
        title="Close"
      >
        ×
      </button>
    </header>
  );
}

interface CompactionMarkerRowProps {
  msg: HistoryMessage;
  compactionSeq: number;
  onShowEarlier: (seq: number) => void;
  markdownComponents: Components;
}

function CompactionMarkerRow({
  msg,
  compactionSeq,
  onShowEarlier,
  markdownComponents,
}: CompactionMarkerRowProps) {
  return (
    <div
      className="aui-compaction-marker"
      data-testid="archived-compaction-marker"
      data-compaction-seq={compactionSeq}
    >
      <div
        className="aui-compaction-divider"
        role="separator"
        aria-label="Compacted history divider"
      >
        <div className="aui-compaction-divider-line" />
        <span className="aui-compaction-badge">Compaction</span>
        <div className="aui-compaction-divider-line" />
      </div>
      <div className="aui-compaction-card">
        <div className="aui-compaction-card-header">
          <h3 className="aui-compaction-title">Summary of earlier conversation</h3>
          <button
            type="button"
            className="aui-compaction-action-btn"
            onClick={() => onShowEarlier(compactionSeq)}
            data-testid="archived-earlier-history-btn"
          >
            Show earlier history
          </button>
        </div>
        <div className="aui-compaction-summary">
          <ReactMarkdown
            remarkPlugins={[remarkGfm]}
            urlTransform={markdownUrlTransform}
            components={markdownComponents}
          >
            {msg.content}
          </ReactMarkdown>
        </div>
      </div>
    </div>
  );
}

interface MessageRowProps {
  msg: HistoryMessage;
  markdownComponents: Components;
}

function MessageRow({ msg, markdownComponents }: MessageRowProps) {
  const roleClass = msg.role || 'assistant';
  return (
    <article
      className={`archived-history-message ${roleClass}`}
      data-testid={`archived-msg-${roleClass}`}
    >
      <span className="archived-history-message-role">{msg.role}</span>
      <div className="archived-history-message-body">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          urlTransform={markdownUrlTransform}
          components={markdownComponents}
        >
          {msg.content}
        </ReactMarkdown>
      </div>
    </article>
  );
}

interface MessagesListProps {
  data: SessionHistoryResult;
  onShowEarlier: (seq: number) => void;
  markdownComponents: Components;
}

function MessagesList({ data, onShowEarlier, markdownComponents }: MessagesListProps) {
  const compaction = data.compaction;
  return (
    <div className="archived-history-messages" data-testid="archived-messages">
      {data.history_snapshot.map((msg: HistoryMessage) => {
        if (compaction && msg.id === compaction.message_id) {
          return (
            <CompactionMarkerRow
              key={msg.id}
              msg={msg}
              compactionSeq={compaction.compaction_seq}
              onShowEarlier={onShowEarlier}
              markdownComponents={markdownComponents}
            />
          );
        }
        return (
          <MessageRow
            key={msg.id}
            msg={msg}
            markdownComponents={markdownComponents}
          />
        );
      })}
    </div>
  );
}

interface PaginationFooterProps {
  prevCursor: string | null;
  nextCursor: string | null;
  onPrev: () => void;
  onNext: () => void;
}

function PaginationFooter({
  prevCursor,
  nextCursor,
  onPrev,
  onNext,
}: PaginationFooterProps) {
  if (!prevCursor && !nextCursor) return null;
  return (
    <footer className="archived-history-pagination" aria-label="Pagination">
      <button
        type="button"
        className="archived-history-page-btn"
        onClick={onPrev}
        disabled={!prevCursor}
        aria-label="Previous page"
      >
        ← Previous page
      </button>
      <button
        type="button"
        className="archived-history-page-btn"
        onClick={onNext}
        disabled={!nextCursor}
        aria-label="Next page"
      >
        Next page →
      </button>
    </footer>
  );
}

export function ArchivedHistoryViewer({
  initialTarget,
  onClose,
}: ArchivedHistoryViewerProps) {
  const { currentTarget, canGoBack, goBack, pushTarget } = useHistoryNavigation(initialTarget);
  const activeRequestIdRef = useRef<number>(0);

  const {
    loadState,
    handleRetry,
    handleNextPage,
    handlePrevPage,
    resetCursor,
  } = useHistoryPageLoader(currentTarget, activeRequestIdRef);

  const dialogRef = useRef<HTMLElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  useHistoryDialog(dialogRef, closeButtonRef, onClose);

  const markdownComponents = useMemo<Components>(() => ({
    a: MarkdownLink,
    code: MarkdownCode,
    table: ({ node: _node, ...props }) => (
      <div className="overflow-x-auto" tabIndex={0} role="region" aria-label="Table content">
        <table {...props} />
      </div>
    ),
  }), []);

  const handleBack = useCallback(() => {
    goBack();
    resetCursor();
  }, [goBack, resetCursor]);

  const handleOpenEarlier = useCallback((compactionSeq: number) => {
    pushTarget(compactionSeq);
    resetCursor();
  }, [pushTarget, resetCursor]);

  // Focus retention: keep focus inside the dialog when page data settles
  const isFirstLoadRef = useRef(true);
  useEffect(() => {
    if (loadState.status === 'loading') return;
    if (isFirstLoadRef.current) {
      isFirstLoadRef.current = false;
      return;
    }
    const dialog = dialogRef.current;
    const activeEl = globalThis.document.activeElement;
    if (!dialog || !activeEl || !dialog.contains(activeEl) || activeEl === globalThis.document.body) {
      contentRef.current?.focus();
    }
  }, [loadState.status]);

  const closeOnBackdrop = (event: MouseEvent<HTMLDivElement>) => {
    if (event.target === event.currentTarget) onClose();
  };

  const title = currentTarget.title || `History before compaction ${currentTarget.beforeCompaction}`;

  return (
    <div
      className="archived-history-backdrop"
      role="presentation"
      onMouseDown={closeOnBackdrop}
      data-testid="archived-history-viewer"
    >
      <section
        ref={dialogRef}
        className="archived-history-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="archived-history-title"
      >
        <ViewerHeader
          title={title}
          agent={currentTarget.agent}
          session={currentTarget.session}
          canGoBack={canGoBack}
          closeButtonRef={closeButtonRef}
          onBack={handleBack}
          onClose={onClose}
        />

        <div
          ref={contentRef}
          tabIndex={-1}
          className="archived-history-content"
          aria-label="Archived transcript content"
          aria-busy={loadState.status === 'loading'}
        >
          {loadState.status === 'loading' && (
            <div className="archived-history-loading" role="status">
              <span className="aui-spinner" aria-hidden="true"><span /></span>
              <span>Loading archived history…</span>
            </div>
          )}

          {loadState.status === 'error' && (
            <div className="aui-error archived-history-error" role="alert">
              <strong>Unable to load archived history.</strong>
              <span>{loadState.message}</span>
              <button
                type="button"
                className="aui-error-retry-btn"
                onClick={handleRetry}
              >
                Retry
              </button>
            </div>
          )}

          {loadState.status === 'loaded' && (
            <>
              <MessagesList
                data={loadState.data}
                onShowEarlier={handleOpenEarlier}
                markdownComponents={markdownComponents}
              />
              <PaginationFooter
                prevCursor={loadState.data.prev_cursor}
                nextCursor={loadState.data.next_cursor}
                onPrev={handlePrevPage}
                onNext={handleNextPage}
              />
            </>
          )}
        </div>
      </section>
    </div>
  );
}
