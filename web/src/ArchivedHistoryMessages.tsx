import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import type { HistoryMessage, SessionHistoryResult } from './types';
import { markdownUrlTransform } from './markdownUrl';
import { splitThinkBlocks } from './lib/thinkBlocks';
import { ThinkingBlock } from './components/ThinkingBlock';

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
  reasoning?: string[];
  hasHistoryReasoning: boolean;
  markdownComponents: Components;
}

function MessageRow({ msg, reasoning, hasHistoryReasoning, markdownComponents }: MessageRowProps) {
  const roleClass = msg.role || 'assistant';
  const content = msg.content || '';

  if (msg.role !== 'assistant') {
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
            {content}
          </ReactMarkdown>
        </div>
      </article>
    );
  }

  if (hasHistoryReasoning) {
    const thoughts = Array.isArray(reasoning)
      ? reasoning.filter((thought): thought is string => typeof thought === 'string' && thought.trim().length > 0)
      : [];

    return (
      <article
        className={`archived-history-message ${roleClass}`}
        data-testid={`archived-msg-${roleClass}`}
      >
        <span className="archived-history-message-role">{msg.role}</span>
        <div className="archived-history-message-body">
          {thoughts.map((thought, idx) => (
            <ThinkingBlock
              key={`thought-${idx}`}
              text={thought}
              isStreaming={false}
            />
          ))}
          {content.length > 0 && (
            <ReactMarkdown
              remarkPlugins={[remarkGfm]}
              urlTransform={markdownUrlTransform}
              components={markdownComponents}
            >
              {content}
            </ReactMarkdown>
          )}
        </div>
      </article>
    );
  }

  // Legacy fallback for older servers that do not include history_reasoning.
  const segments = splitThinkBlocks(content);

  return (
    <article
      className={`archived-history-message ${roleClass}`}
      data-testid={`archived-msg-${roleClass}`}
    >
      <span className="archived-history-message-role">{msg.role}</span>
      <div className="archived-history-message-body">
        {segments.map((segment, idx) => {
          if (segment.type === 'thought') {
            return (
              <ThinkingBlock
                key={`thought-${idx}`}
                text={segment.text}
                isStreaming={false}
              />
            );
          }
          return (
            <ReactMarkdown
              key={`text-${idx}`}
              remarkPlugins={[remarkGfm]}
              urlTransform={markdownUrlTransform}
              components={markdownComponents}
            >
              {segment.text}
            </ReactMarkdown>
          );
        })}
      </div>
    </article>
  );
}

interface MessagesListProps {
  data: SessionHistoryResult;
  onShowEarlier: (seq: number) => void;
  markdownComponents: Components;
}

export function MessagesList({ data, onShowEarlier, markdownComponents }: MessagesListProps) {
  const compaction = data.compaction;
  const historyReasoning =
    data.history_reasoning !== null &&
    typeof data.history_reasoning === 'object' &&
    !Array.isArray(data.history_reasoning)
      ? data.history_reasoning
      : undefined;

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
        const reasoning =
          historyReasoning && typeof msg.id === 'string' && Object.hasOwn(historyReasoning, msg.id)
            ? historyReasoning[msg.id]
            : undefined;
        return (
          <MessageRow
            key={msg.id}
            msg={msg}
            reasoning={reasoning}
            hasHistoryReasoning={historyReasoning !== undefined}
            markdownComponents={markdownComponents}
          />
        );
      })}
    </div>
  );
}
