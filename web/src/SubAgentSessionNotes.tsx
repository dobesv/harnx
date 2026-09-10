import { useEffect, useState, useMemo, useContext, useRef } from 'react';
import type { KeyboardEvent } from 'react';
import type { SubAgentNote } from './subAgentNotes';
import { HarnxHttpAgent } from './ChatProvider';
import { SubAgentNotesContext } from './SubAgentNotesContext';

export interface SubAgentSessionNotesProps {
  notes: SubAgentNote[];
  onOpen: (agent: string, sessionId: string) => void;
}

import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { AssistantRuntimeProvider } from '@assistant-ui/react';
import { RuntimeSessionSubscriber } from './RuntimeSessionSubscriber';

function ChildMetricsSubscriber({ note, dispatch }: { note: SubAgentNote, dispatch: (event: unknown) => void }) {
  const noteRef = useRef(note);
  const dispatchRef = useRef(dispatch);
  const toolCallCountRef = useRef(note.toolCallCount || 0);

  useEffect(() => {
    noteRef.current = note;
    dispatchRef.current = dispatch;
    if (note.toolCallCount > toolCallCountRef.current) {
      toolCallCountRef.current = note.toolCallCount;
    }
  }, [note, dispatch]);

  const agent = useMemo(() => {
    return new HarnxHttpAgent({
      url: `/v1/agents/${encodeURIComponent(note.agent)}/sessions/${encodeURIComponent(note.sessionId)}`,
      onStatus: () => {},
      onRunFailed: () => {},
      onUsage: (usage) => {
        const currentNote = noteRef.current;
        const elapsed = currentNote.startedAtMs
          ? Math.max(0, Date.now() - currentNote.startedAtMs)
          : currentNote.elapsedMs + Math.max(0, Date.now() - currentNote.updatedAtMs);

        dispatchRef.current({
          type: 'CUSTOM',
          name: 'sub_agent_progress',
          value: {
            invocation_id: currentNote.invocationId,
            tool_call_id: currentNote.toolCallId,
            agent: currentNote.agent,
            session_id: currentNote.sessionId,
            started_at: currentNote.startedAtMs,
            elapsed_ms: elapsed,
            tool_call_count: toolCallCountRef.current,
            status: currentNote.status,
            usage: {
              input_tokens: usage.input,
              output_tokens: usage.output,
              cached_tokens: usage.cached ?? 0,
            }
          }
        });
      },
      onToolSummary: () => {
        toolCallCountRef.current++;
      },
      onSubAgentEvent: (event: any) => {
        if (event?.type === 'RUN_FINISHED' || event?.type === 'RUN_ERROR') {
          const now = Date.now();
          const graceMs = 5000;
          const currentNote = noteRef.current;
          if (currentNote.startedAtMs && (now - currentNote.startedAtMs < graceMs)) {
            return; // within grace period
          }
          
          const isError = event.type === 'RUN_ERROR' || event.result?.status === 'failed';

          dispatchRef.current({
            type: 'CHILD_TERMINAL',
            invocationId: currentNote.invocationId,
            toolCallId: currentNote.toolCallId,
            status: isError ? 'failed' : 'done',
          });
        }
      },
    });
  }, [note.agent, note.sessionId]);

  const runtime = useAgUiRuntime({ agent });

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <RuntimeSessionSubscriber
        enabled={true}
        eventsUrl={`/v1/agents/${encodeURIComponent(note.agent)}/sessions/${encodeURIComponent(note.sessionId)}/events`}
      />
    </AssistantRuntimeProvider>
  );
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
  if (note.status !== 'running') {
    return note.elapsedMs;
  }
  return note.startedAtMs
    ? Math.max(0, nowMs - note.startedAtMs)
    : note.elapsedMs + Math.max(0, nowMs - note.updatedAtMs);
}

function formatElapsed(value: number) {
  const seconds = Math.floor(value / 1000);
  return `${seconds}s`;
}

function formatTokens(value: number) {
  return value.toLocaleString();
}

export function SubAgentSessionNotes({ notes, onOpen }: SubAgentSessionNotesProps) {
  const { dispatch } = useContext(SubAgentNotesContext);
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
            {note.status === 'running' && (
              <ChildMetricsSubscriber note={note} dispatch={dispatch} />
            )}
          </button>
        );
      })}
    </div>
  );
}
