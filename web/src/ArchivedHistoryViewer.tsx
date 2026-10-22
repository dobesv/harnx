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

const FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function useHistoryDialog(
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

      if (event.key !== 'Tab') return;

      const dialog = dialogRef.current;
      if (!dialog) return;

      const focusables = Array.from(
        dialog.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR),
      );

      if (focusables.length === 0) {
        event.preventDefault();
        return;
      }

      const firstElement = focusables[0];
      const lastElement = focusables[focusables.length - 1];
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

function MarkdownCode({
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

export function ArchivedHistoryViewer({
  initialTarget,
  onClose,
}: ArchivedHistoryViewerProps) {
  // Navigation stack for nested compactions (walking back through compactions)
  const [targetStack, setTargetStack] = useState<ArchivedHistoryTarget[]>([initialTarget]);
  const currentTarget = targetStack[targetStack.length - 1] ?? initialTarget;
  const canGoBack = targetStack.length > 1;

  // Sync internal stack if initialTarget prop changes (e.g. in tests)
  const prevInitialTargetRef = useRef(initialTarget);
  if (
    prevInitialTargetRef.current.agent !== initialTarget.agent ||
    prevInitialTargetRef.current.session !== initialTarget.session ||
    prevInitialTargetRef.current.beforeCompaction !== initialTarget.beforeCompaction
  ) {
    prevInitialTargetRef.current = initialTarget;
    setTargetStack([initialTarget]);
  }

  // Window page state
  const [loadState, setLoadState] = useState<WindowLoadState>({ status: 'loading' });
  const [activeCursor, setActiveCursor] = useState<string | null>(null);

  const dialogRef = useRef<HTMLElement>(null);
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

  // Request token ref to ignore late/superseded responses
  const activeRequestIdRef = useRef<number>(0);

  // Load a page
  const loadPage = useCallback(
    (target: ArchivedHistoryTarget, cursor: string | null, signal?: AbortSignal) => {
      const requestId = ++activeRequestIdRef.current;
      setLoadState({ status: 'loading' });

      const params = cursor
        ? { cursor }
        : { before_compaction: target.beforeCompaction };

      getSessionHistory(target.agent, target.session, params, { signal })
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
    [],
  );

  useEffect(() => {
    const controller = new AbortController();
    loadPage(currentTarget, activeCursor, controller.signal);
    return () => {
      controller.abort();
    };
  }, [currentTarget, activeCursor, loadPage]);

  const handleRetry = useCallback(() => {
    loadPage(currentTarget, activeCursor);
  }, [currentTarget, activeCursor, loadPage]);

  const goBack = useCallback(() => {
    if (targetStack.length <= 1) return;
    setTargetStack((prev) => prev.slice(0, -1));
    setActiveCursor(null);
  }, [targetStack.length]);

  const openEarlierCompaction = useCallback(
    (compactionSeq: number) => {
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
      setActiveCursor(null);
    },
    [initialTarget.agent, initialTarget.session],
  );

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
        <header className="archived-history-header">
          <button
            type="button"
            className="archived-history-back"
            onClick={goBack}
            disabled={!canGoBack}
            aria-label="Back to more recent archived history"
            title="Back"
          >
            ←
          </button>
          <div className="archived-history-heading">
            <h2 id="archived-history-title">{title}</h2>
            <span className="archived-history-meta">
              Archived read-only transcript • {currentTarget.agent}/{currentTarget.session}
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

        <div className="archived-history-content">
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
              <div className="archived-history-messages" data-testid="archived-messages">
                {loadState.data.history_snapshot.map((msg: HistoryMessage) => {
                  const isCompactionMarker =
                    loadState.data.compaction &&
                    msg.id === loadState.data.compaction.message_id;

                  if (isCompactionMarker && loadState.data.compaction) {
                    return (
                      <div
                        key={msg.id}
                        className="aui-compaction-marker"
                        data-testid="archived-compaction-marker"
                        data-compaction-seq={loadState.data.compaction.compaction_seq}
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
                              onClick={() => openEarlierCompaction(loadState.data.compaction!.compaction_seq)}
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

                  const roleClass = msg.role || 'assistant';
                  return (
                    <article
                      key={msg.id}
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
                })}
              </div>

              {(loadState.data.prev_cursor || loadState.data.next_cursor) && (
                <footer className="archived-history-pagination" aria-label="Pagination">
                  <button
                    type="button"
                    className="archived-history-page-btn"
                    onClick={handlePrevPage}
                    disabled={!loadState.data.prev_cursor}
                    aria-label="Previous page"
                  >
                    ← Previous page
                  </button>
                  <button
                    type="button"
                    className="archived-history-page-btn"
                    onClick={handleNextPage}
                    disabled={!loadState.data.next_cursor}
                    aria-label="Next page"
                  >
                    Next page →
                  </button>
                </footer>
              )}
            </>
          )}
        </div>
      </section>
    </div>
  );
}
