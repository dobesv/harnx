import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  type ComponentPropsWithoutRef,
  type MouseEvent,
  type RefObject,
} from 'react';
import { type Components } from 'react-markdown';
import { PrismAsyncLight as SyntaxHighlighter } from 'react-syntax-highlighter';
import type { ArchivedHistoryTarget } from './ArchivedHistoryContext';
import { MarkdownLink } from './markdownLink';
import { useHistoryDialog } from './useHistoryDialog';
import { useHistoryPageLoader } from './useHistoryPageLoader';
import { MessagesList } from './ArchivedHistoryMessages';
import { useHistoryNavigation } from './useHistoryNavigation';

export interface ArchivedHistoryViewerProps {
  initialTarget: ArchivedHistoryTarget;
  onClose: () => void;
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

function useMarkdownTableComponents(): Components {
  return useMemo<Components>(() => ({
    a: MarkdownLink,
    code: MarkdownCode,
    table: ({ node: _node, ...props }) => (
      <div className="overflow-x-auto" tabIndex={0} role="region" aria-label="Table content">
        <table {...props} />
      </div>
    ),
  }), []);
}

interface HistoryViewerContentProps {
  contentRef: RefObject<HTMLDivElement | null>;
  loadState: ReturnType<typeof useHistoryPageLoader>['loadState'];
  onRetry: () => void;
  onOpenEarlier: (seq: number) => void;
  onPrevPage: () => void;
  onNextPage: () => void;
  markdownComponents: Components;
}

function HistoryViewerContent({
  contentRef,
  loadState,
  onRetry,
  onOpenEarlier,
  onPrevPage,
  onNextPage,
  markdownComponents,
}: HistoryViewerContentProps) {
  return (
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
            onClick={onRetry}
          >
            Retry
          </button>
        </div>
      )}

      {loadState.status === 'loaded' && (
        <>
          <MessagesList
            data={loadState.data}
            onShowEarlier={onOpenEarlier}
            markdownComponents={markdownComponents}
          />
          <PaginationFooter
            prevCursor={loadState.data.prev_cursor}
            nextCursor={loadState.data.next_cursor}
            onPrev={onPrevPage}
            onNext={onNextPage}
          />
        </>
      )}
    </div>
  );
}

function shouldRecoverFocus(dialog: HTMLElement | null, activeEl: Element | null): boolean {
  if (!dialog || !activeEl) return true;
  return !dialog.contains(activeEl) || activeEl === globalThis.document.body;
}

function useFocusRetention(
  dialogRef: RefObject<HTMLElement | null>,
  contentRef: RefObject<HTMLDivElement | null>,
  isLoading: boolean,
): void {
  const isFirstLoadRef = useRef(true);

  useEffect(() => {
    if (isLoading) return;
    if (isFirstLoadRef.current) {
      isFirstLoadRef.current = false;
      return;
    }
    if (shouldRecoverFocus(dialogRef.current, globalThis.document.activeElement)) {
      contentRef.current?.focus();
    }
  }, [isLoading, dialogRef, contentRef]);
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
  useFocusRetention(dialogRef, contentRef, loadState.status === 'loading');

  const markdownComponents = useMarkdownTableComponents();

  const handleBack = useCallback(() => {
    goBack();
    resetCursor();
  }, [goBack, resetCursor]);

  const handleOpenEarlier = useCallback((compactionSeq: number) => {
    pushTarget(compactionSeq);
    resetCursor();
  }, [pushTarget, resetCursor]);

  const onNext = useCallback(() => {
    if (loadState.status === 'loaded' && loadState.data.next_cursor) {
      handleNextPage(loadState.data.next_cursor);
    }
  }, [loadState, handleNextPage]);

  const onPrev = useCallback(() => {
    if (loadState.status === 'loaded' && loadState.data.prev_cursor) {
      handlePrevPage(loadState.data.prev_cursor);
    }
  }, [loadState, handlePrevPage]);

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

        <HistoryViewerContent
          contentRef={contentRef}
          loadState={loadState}
          onRetry={handleRetry}
          onOpenEarlier={handleOpenEarlier}
          onPrevPage={onPrev}
          onNextPage={onNext}
          markdownComponents={markdownComponents}
        />
      </section>
    </div>
  );
}
