import { useEffect, useState } from 'react';
import type { KeyboardEvent } from 'react';
import type { SubAgentNote } from './subAgentNotes';

export interface SubAgentSessionNotesProps {
  notes: SubAgentNote[];
  onOpen: (agent: string, sessionId: string) => void;
}

const STATUS_LABEL = {
  running: 'Running',
  done: 'Done',
  failed: 'Failed',
} as const;

function activateOnKey(
  event: KeyboardEvent<HTMLButtonElement>,
  action: () => void,
) {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    action();
  }
}

function elapsedMs(note: SubAgentNote, nowMs: number) {
  return note.status === 'running'
    ? note.elapsedMs + Math.max(0, nowMs - note.updatedAtMs)
    : note.elapsedMs;
}

function formatElapsed(value: number) {
  const seconds = Math.floor(value / 1000);
  return `${seconds}s`;
}

function formatTokens(value: number) {
  return value.toLocaleString();
}

export function SubAgentSessionNotes({ notes, onOpen }: SubAgentSessionNotesProps) {
  const [clockMs, setClockMs] = useState(0);
  const hasRunning = notes.some((note) => note.status === 'running');
  useEffect(() => {
    if (!hasRunning) return undefined;
    const timer = window.setInterval(() => setClockMs(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [hasRunning]);
  const nowMs = clockMs || Math.max(0, ...notes.map((note) => note.updatedAtMs));

  if (notes.length === 0) return null;

  return (
    <div className="aui-sub-agent-notes" aria-label="Sub-agent sessions">
      {notes.map((note) => {
        const statusLabel = STATUS_LABEL[note.status];
        const open = () => onOpen(note.agent, note.sessionId);
        const displayedElapsedMs = elapsedMs(note, nowMs);
        return (
          <button
            type="button"
            className="aui-sub-agent-note"
            data-status={note.status}
            data-elapsed-ms={Math.floor(displayedElapsedMs)}
            key={note.id}
            aria-label={`Open ${note.agent} sub-agent session ${note.sessionId} (${statusLabel.toLowerCase()})`}
            onClick={open}
            onKeyDown={(event) => activateOnKey(event, open)}
          >
            <span className="aui-sub-agent-identity">
              <span className="aui-sub-agent-identity-line">
                <span className="aui-sub-agent-name">{note.agent}</span>
                <span className="aui-sub-agent-session">{note.sessionId}</span>
              </span>
              <span className="aui-sub-agent-metrics">
                <span>{formatElapsed(displayedElapsedMs)}</span>
                <span>in {formatTokens(note.inputTokens)}</span>
                <span>out {formatTokens(note.outputTokens)}</span>
                <span>cache {formatTokens(note.cachedTokens)}</span>
                <span>tools {note.toolCallCount}</span>
              </span>
            </span>
            <span className={`aui-sub-agent-status aui-sub-agent-status-${note.status}`}>
              <span className="aui-sub-agent-status-icon" aria-hidden="true" />
              {statusLabel}
            </span>
          </button>
        );
      })}
    </div>
  );
}
