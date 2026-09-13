import { cancel, sessionControl } from './api';
import { isAbortError } from './httpClient';
import { useEffect, useRef, useState } from 'react';
import type { KeyboardEvent } from 'react';
import type { SubAgentNote } from './subAgentNotes';
import type { SubAgentSessionNotesProps } from './SubAgentSessionNotes';

const STATUS_LABEL = {
  running: 'Running',
  done: 'Done',
  failed: 'Failed',
  cancelling: 'Cancelling',
  cancelled: 'Cancelled',
  unconfirmed: 'Unconfirmed',
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
  if (note.status !== 'running') return note.elapsedMs;
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

function deriveSubAgentStatus(
  executionState: string | undefined,
  cancellationDisposition: string | undefined
): { status?: SubAgentNote['status']; recordContact?: boolean } {
  if (executionState === 'completed') return { status: 'done' };
  if (executionState === 'cancelled' || cancellationDisposition === 'cancelled') {
    return { status: 'cancelled' };
  }
  if (cancellationDisposition === 'unconfirmed') return { status: 'unconfirmed' };
  if (cancellationDisposition && cancellationDisposition !== 'idle') {
    return { status: 'cancelling', recordContact: true };
  }
  return {};
}

function handleSubAgentRefreshFailure({
  err,
  lastContact,
  clearActiveExecution,
  markUnconfirmed,
}: {
  err: unknown;
  lastContact: number | null;
  clearActiveExecution: () => void;
  markUnconfirmed: () => void;
}): void {
  if (!isAbortError(err)) {
    clearActiveExecution();
  }
  if (lastContact !== null && Date.now() - lastContact >= 5000) {
    markUnconfirmed();
  }
}

export function SubAgentRow({ note, nowMs, onOpen }: { note: SubAgentNote; nowMs: number; onOpen: SubAgentSessionNotesProps['onOpen'] }) {
  const [localStatus, setLocalStatus] = useState<SubAgentNote['status']>();
  const [activeExecution, setActiveExecution] = useState<string>();
  const [error, setError] = useState<string>();
  const [stoppedAt, setStoppedAt] = useState<number>();
  const requestedAt = useRef<number | null>(null);
  const lastStatusAt = useRef<number | null>(null);
  const canStop = note.status === 'running' && !localStatus && !!note.invocationId && activeExecution === note.invocationId;
  useEffect(() => {
    if (!note.invocationId || ['done', 'failed', 'cancelled'].includes(note.status)) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    const refresh = async () => {
      try {
        const state = await sessionControl(note.agent, note.sessionId, { signal: controller.signal });
        if (disposed) return;
        const isRunning = ['preparing', 'running'].includes(state.execution_state ?? '');
        setActiveExecution(isRunning ? state.execution_id : undefined);

        if (state.execution_id === note.invocationId) {
          const derived = deriveSubAgentStatus(
            state.execution_state,
            state.state.cancellation?.disposition
          );
          if (derived.recordContact) lastStatusAt.current = Date.now();
          if (derived.status) setLocalStatus(derived.status);
        }
      } catch (err) {
        if (!disposed) {
          handleSubAgentRefreshFailure({
            err,
            lastContact: lastStatusAt.current ?? requestedAt.current,
            clearActiveExecution: () => setActiveExecution(undefined),
            markUnconfirmed: () => setLocalStatus('unconfirmed'),
          });
        }
      }
      if (!disposed) timer = setTimeout(refresh, 500);
    };
    void refresh();
    return () => {
      disposed = true;
      clearTimeout(timer);
      controller.abort();
    };
  }, [note.agent, note.sessionId, note.invocationId, note.status]);
  const stop = async () => {
    if (!note.invocationId) return;
    requestedAt.current = Date.now();
    setStoppedAt(previous => previous ?? Date.now());
    lastStatusAt.current = null;
    setLocalStatus('cancelling');
    setError(undefined);
    try {
      const receipt = await cancel(note.agent, note.sessionId, note.invocationId);
      if (receipt.disposition === 'idle') { setActiveExecution(undefined); setLocalStatus(undefined); }
      else if (receipt.disposition === 'cancelled') setLocalStatus('cancelled');
      else if (receipt.disposition === 'unconfirmed') setLocalStatus('unconfirmed');
    } catch (error) {
      setLocalStatus('unconfirmed');
      if (!isAbortError(error)) {
        setError(String(error));
      }
    }
  };
        const statusLabel = STATUS_LABEL[localStatus ?? note.status];
        const open = () => onOpen(note.agent, note.sessionId);
        const displayedElapsedMs = elapsedMs(note, localStatus ? (stoppedAt ?? note.updatedAtMs) : nowMs);
        return (
          <div className="aui-sub-agent-row">
          <button
            type="button"
            className="aui-sub-agent-note"
            data-status={localStatus ?? note.status}
            data-elapsed-ms={Math.floor(displayedElapsedMs)}
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
            <span className={`aui-sub-agent-status aui-sub-agent-status-${localStatus ?? note.status}`}>
              <span className="aui-sub-agent-status-icon" aria-hidden="true" />
              {statusLabel}
            </span>
          </button>
          {canStop && <button type="button" className="aui-sub-agent-stop" aria-label={`Stop ${note.agent} sub-agent session ${note.sessionId}`} onClick={() => void stop()}>Stop</button>}
          {localStatus === 'unconfirmed' && <button type="button" aria-label={`Retry stopping ${note.agent}`} onClick={() => void stop()}>Retry</button>}
          {error && <span role="alert">{error}</span>}
          </div>
        );

}
