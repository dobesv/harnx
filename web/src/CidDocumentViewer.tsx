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
import { fetchCidContent } from './api';
import type { CidDocumentTarget } from './CidDocumentContext';
import { MarkdownLink } from './markdownLink';
import { markdownUrlTransform } from './markdownUrl';

export interface CidDocumentViewerProps {
  cid: string;
  title?: string;
  onClose: () => void;
}

type LoadedDocument = {
  mimeType: string;
  text: string;
  etag?: string;
};

type DocumentLoad =
  | { cid: string; status: 'loading' }
  | { cid: string; status: 'loaded'; content: LoadedDocument }
  | { cid: string; status: 'error'; message: string };

interface DocumentNavigation {
  current: CidDocumentTarget;
  canGoBack: boolean;
  navigate: (target: CidDocumentTarget) => void;
  goBack: () => void;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function useCidDocumentLoader(cid: string): DocumentLoad {
  const [load, setLoad] = useState<DocumentLoad>({ cid, status: 'loading' });

  useEffect(() => {
    let active = true;
    fetchCidContent(cid)
      .then((content) => {
        if (active) setLoad({ cid, status: 'loaded', content });
      })
      .catch((fetchError: unknown) => {
        if (active) setLoad({ cid, status: 'error', message: errorMessage(fetchError) });
      });

    return () => {
      active = false;
    };
  }, [cid]);

  return load;
}

function useCidDocumentNavigation(initial: CidDocumentTarget): DocumentNavigation {
  const [current, setCurrent] = useState(initial);
  const [history, setHistory] = useState<CidDocumentTarget[]>([]);

  const navigate = useCallback((target: CidDocumentTarget) => {
    if (target.cid === current.cid) return;
    setHistory((previous) => [...previous, current]);
    setCurrent(target);
  }, [current]);

  const goBack = useCallback(() => {
    const prior = history.at(-1);
    if (!prior) return;
    setHistory((previous) => previous.slice(0, -1));
    setCurrent(prior);
  }, [history]);

  return { current, canGoBack: history.length > 0, navigate, goBack };
}

function useDocumentDialog(
  closeButtonRef: RefObject<HTMLButtonElement | null>,
  onClose: () => void,
): void {
  useEffect(() => {
    const previouslyFocused = globalThis.document.activeElement;
    closeButtonRef.current?.focus();
    return () => {
      if (previouslyFocused instanceof HTMLElement) previouslyFocused.focus();
    };
  }, [closeButtonRef]);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      onClose();
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [onClose]);
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

function useMarkdownComponents(
  navigate: (target: CidDocumentTarget) => void,
): Components {
  return useMemo<Components>(() => ({
    a: (props) => <MarkdownLink {...props} onOpenCid={navigate} />,
    code: MarkdownCode,
    table: ({ node: _node, ...props }) => (
      <div className="overflow-x-auto">
        <table {...props} />
      </div>
    ),
  }), [navigate]);
}

interface ViewerHeaderProps {
  current: CidDocumentTarget;
  canGoBack: boolean;
  closeButtonRef: RefObject<HTMLButtonElement | null>;
  onBack: () => void;
  onClose: () => void;
}

function ViewerHeader({
  current,
  canGoBack,
  closeButtonRef,
  onBack,
  onClose,
}: ViewerHeaderProps) {
  return (
    <header className="cid-document-header">
      <button
        type="button"
        className="cid-document-back"
        onClick={onBack}
        disabled={!canGoBack}
        aria-label="Back"
      >
        ←
      </button>
      <div className="cid-document-heading">
        <h2 id="cid-document-title">{current.title || current.cid}</h2>
        <code id="cid-document-url" title={current.cid}>{current.cid}</code>
      </div>
      <button
        ref={closeButtonRef}
        type="button"
        className="cid-document-close"
        onClick={onClose}
        aria-label="Close document viewer"
        title="Close"
      >
        ×
      </button>
    </header>
  );
}

function ViewerBody({
  cid,
  load,
  markdownComponents,
}: {
  cid: string;
  load: DocumentLoad;
  markdownComponents: Components;
}) {
  if (load.cid !== cid || load.status === 'loading') {
    return (
      <div className="cid-document-loading" role="status">
        <span className="aui-spinner" aria-hidden="true"><span /></span>
        <span>Loading document…</span>
      </div>
    );
  }
  if (load.status === 'error') {
    return (
      <div className="aui-error cid-document-error" role="alert">
        <strong>Unable to load document.</strong>
        <span>{load.message}</span>
      </div>
    );
  }

  return (
    <article className="cid-document-markdown" data-mime-type={load.content.mimeType}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        urlTransform={markdownUrlTransform}
        components={markdownComponents}
      >
        {load.content.text}
      </ReactMarkdown>
    </article>
  );
}

function closeOnBackdrop(event: MouseEvent<HTMLDivElement>, onClose: () => void): void {
  if (event.target === event.currentTarget) onClose();
}

export function CidDocumentViewer({ cid, title, onClose }: CidDocumentViewerProps) {
  const navigation = useCidDocumentNavigation({ cid, title });
  const load = useCidDocumentLoader(navigation.current.cid);
  const markdownComponents = useMarkdownComponents(navigation.navigate);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  useDocumentDialog(closeButtonRef, onClose);

  return (
    <div
      className="cid-document-backdrop"
      role="presentation"
      onMouseDown={(event) => closeOnBackdrop(event, onClose)}
    >
      <section
        className="cid-document-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="cid-document-title"
        aria-describedby="cid-document-url"
      >
        <ViewerHeader
          current={navigation.current}
          canGoBack={navigation.canGoBack}
          closeButtonRef={closeButtonRef}
          onBack={navigation.goBack}
          onClose={onClose}
        />
        <div className="cid-document-content">
          <ViewerBody
            cid={navigation.current.cid}
            load={load}
            markdownComponents={markdownComponents}
          />
        </div>
      </section>
    </div>
  );
}
