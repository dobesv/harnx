import type { ComponentPropsWithoutRef } from 'react';
import { MessagePrimitive } from '@assistant-ui/react';
import { MarkdownTextPrimitive } from '@assistant-ui/react-markdown';
import { makeLightAsyncSyntaxHighlighter } from '@assistant-ui/react-syntax-highlighter';
import remarkGfm from 'remark-gfm';
import type { CompactionMetadata } from './compactionMetadata';
import { MarkdownLink } from './markdownLink';
import { markdownUrlTransform } from './markdownUrl';
import { useArchivedHistory } from './ArchivedHistoryContext';
import { useCompactionMetadata } from './CompactionMetadataContext';

const SyntaxHighlighter = makeLightAsyncSyntaxHighlighter({ useInlineStyles: false });

export interface CompactionMarkerProps {
  metadata: CompactionMetadata;
}

export function CompactionMarker({ metadata }: CompactionMarkerProps) {
  const historyContext = useArchivedHistory();
  const metadataContext = useCompactionMetadata();

  const handleShowEarlierHistory = () => {
    if (historyContext && metadataContext?.agent && metadataContext?.session) {
      historyContext.openHistory({
        agent: metadataContext.agent,
        session: metadataContext.session,
        beforeCompaction: metadata.compaction_seq,
        title: `History before compaction ${metadata.compaction_seq}`,
      });
    }
  };

  return (
    <MessagePrimitive.Root
      className="aui-compaction-marker"
      data-testid="compaction-marker"
      data-compaction-seq={metadata.compaction_seq}
    >
      <div className="aui-compaction-divider" role="separator" aria-label="Compacted history divider">
        <div className="aui-compaction-divider-line" />
        <span className="aui-compaction-badge">Compaction</span>
        <div className="aui-compaction-divider-line" />
      </div>

      <div className="aui-compaction-card">
        <div className="aui-compaction-card-header">
          <h3 className="aui-compaction-title">Summary of earlier conversation</h3>
          {historyContext && metadataContext?.agent && metadataContext?.session ? (
            <button
              type="button"
              className="aui-compaction-action-btn"
              onClick={handleShowEarlierHistory}
              data-testid="show-earlier-history-btn"
            >
              Show earlier history
            </button>
          ) : (
            <span className="aui-compaction-hint">Earlier history archived</span>
          )}
        </div>
        <div className="aui-compaction-summary" data-testid="compaction-summary">
          <MessagePrimitive.Content
            components={{
              Text: () => (
                <MarkdownTextPrimitive
                  remarkPlugins={[remarkGfm]}
                  urlTransform={markdownUrlTransform}
                  components={{
                    SyntaxHighlighter,
                    a: MarkdownLink,
                    table: ({ node: _node, ...props }: ComponentPropsWithoutRef<'table'> & { node?: unknown }) => (
                      <div className="overflow-x-auto" tabIndex={0} role="region" aria-label="Table">
                        <table {...props} />
                      </div>
                    ),
                  }}
                />
              ),
            }}
          />
        </div>
      </div>
    </MessagePrimitive.Root>
  );
}
