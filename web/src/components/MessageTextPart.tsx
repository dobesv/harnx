import React, { useMemo } from 'react';
import {
  useMessagePartText,
  TextMessagePartProvider,
  useSmooth,
} from '@assistant-ui/react';
import { MarkdownTextPrimitive } from '@assistant-ui/react-markdown';
import { makeLightAsyncSyntaxHighlighter } from '@assistant-ui/react-syntax-highlighter';
import remarkGfm from 'remark-gfm';
import { hasThinkTags, hasUnclosedThinkOpener, resolveThinkSegments } from '../lib/thinkBlocks';
import { ThinkingBlock } from './ThinkingBlock';
import { MarkdownLink } from '../markdownLink';
import { markdownUrlTransform } from '../markdownUrl';

const SyntaxHighlighter = makeLightAsyncSyntaxHighlighter({ useInlineStyles: false });

const markdownComponents = {
  SyntaxHighlighter,
  a: MarkdownLink,
  table: ({ node: _node, ...props }: any) => (
    <div className="overflow-x-auto">
      <table {...props} />
    </div>
  ),
};

export interface MessageTextPartProps {
  className?: string;
  [key: string]: unknown;
}

export const MessageTextPart: React.FC<MessageTextPartProps> = (props) => {
  const part = useMessagePartText();
  const { text: smoothedText, status } = useSmooth(part, true);
  const isRunning = status.type === 'running';

  const shouldSegment = useMemo(() => hasThinkTags(smoothedText), [smoothedText]);
  if (!shouldSegment) {
    return (
      <MarkdownTextPrimitive
        remarkPlugins={[remarkGfm]}
        urlTransform={markdownUrlTransform}
        components={markdownComponents}
        {...props}
      />
    );
  }

  const segments = resolveThinkSegments(smoothedText, { isStreaming: isRunning });

  return (
    <div data-status={status.type} className={props.className}>
      {segments.map((segment, idx) => {
        if (segment.type === 'thought') {
          const isTrailingSegment = idx === segments.length - 1;
          const isSegmentStreaming = isRunning && isTrailingSegment && hasUnclosedThinkOpener(smoothedText);
          return (
            <ThinkingBlock
              key={`thought-${idx}`}
              text={segment.text}
              isStreaming={isSegmentStreaming}
            />
          );
        }
        return (
          <TextMessagePartProvider
            key={`text-${idx}`}
            text={segment.text}
            isRunning={isRunning}
          >
            <MarkdownTextPrimitive
              smooth={false}
              remarkPlugins={[remarkGfm]}
              urlTransform={markdownUrlTransform}
              components={markdownComponents}
            />
          </TextMessagePartProvider>
        );
      })}
    </div>
  );
};
