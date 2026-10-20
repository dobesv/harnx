import type { ComponentPropsWithoutRef } from 'react';
import { MessagePrimitive } from '@assistant-ui/react';
import { MarkdownTextPrimitive } from '@assistant-ui/react-markdown';
import { makeLightAsyncSyntaxHighlighter } from '@assistant-ui/react-syntax-highlighter';
import remarkGfm from 'remark-gfm';
import type { CompactionMetadata } from './compactionMetadata';
import { MarkdownLink } from './markdownLink';
import { markdownUrlTransform } from './markdownUrl';

const SyntaxHighlighter = makeLightAsyncSyntaxHighlighter({ useInlineStyles: false });

export interface CompactionMarkerProps {
  metadata: CompactionMetadata;
}

export function CompactionMarker({ metadata }: CompactionMarkerProps) {
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
          <span className="aui-compaction-hint">Earlier history archived</span>
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
