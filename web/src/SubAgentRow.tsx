import { cancel, sessionControl } from './api';
import { isAbortError } from './httpClient';
import { formatElapsedMs } from './toolCallPresentation';
import { useEffect, useRef, useState } from 'react';
import { LinkButton } from './LinkButton';
import { OpenInNewIcon } from './icons';
import type { SessionStatus } from './types';
import type { SubAgentNote } from './subAgentNotes';
import type { SubAgentSessionNotesProps } from './SubAgentSessionNotes';

// A row shows the parent's progress note unless the child session itself says
// something the parent cannot know — an approval gate it is parked at, or the
// interrupt that stopped it.
type RowStatus = SubAgentNote['status'] | 'awaiting_approval';

const STATUS_LABEL: Record<RowStatus, string> = {
  running: 'Running',
  done: 'Done',
  failed: 'Failed',
  cancelling: 'Cancelling',
  cancelled: 'Cancelled',
  unconfirmed: 'Unconfirmed',
  awaiting_approval: 'Awaiting approval',
};

function elapsedMs(note: SubAgentNote, nowMs: number) {
  if (note.status !== 'running') return note.elapsedMs;
  return note.startedAtMs
    ? Math.max(0, nowMs - note.startedAtMs)
    : note.elapsedMs + Math.max(0, nowMs - note.updatedAtMs);
}

// Use shared formatElapsedMs from toolCallPresentation

function formatTokens(value: number) {
  return value.toLocaleString();
}

// The child session's own view of its turn, as `session/get` reports it.
// `undefined` leaves the parent's progress note in charge of the row.
function deriveSubAgentStatus(sessionStatus: SessionStatus | undefined): RowStatus | undefined {
  if (sessionStatus === 'interrupted') return 'cancelled';
  if (sessionStatus === 'interrupting') return 'cancelling';
  if (sessionStatus === 'awaiting_approval') return 'awaiting_approval';
  return undefined;
}

function handleSubAgentRefreshFailure({
  err,
  lastContact,
  clearChildStatus,
  markUnconfirmed,
}: {
  err: unknown;
  lastContact: number | null;
  clearChildStatus: () => void;
  markUnconfirmed: () => void;
}): void {
  if (!isAbortError(err)) {
    clearChildStatus();
  }
  if (lastContact !== null && Date.now() - lastContact >= 5000) {
    markUnconfirmed();
  }
}

export function SubAgentRow({ note, nowMs, onOpen }: { note: SubAgentNote; nowMs: number; onOpen: SubAgentSessionNotesProps['onOpen'] }) {
  // What this row asked for, kept apart from what the child session reports:
  // the request is this row's own and sticks, while the reported status is
  // re-read on every poll. Latching the reported one made a row whose session
  // id is reused read `interrupted` from the invocation before it and stay
  // Cancelled — with no Stop — for the whole of the next one.
  const [requestedStop, setRequestedStop] = useState<'cancelling' | 'unconfirmed'>();
  const [childStatus, setChildStatus] = useState<SessionStatus>();
  const [error, setError] = useState<string>();
  const [stoppedAt, setStoppedAt] = useState<number>();
  const requestedAt = useRef<number | null>(null);
  const lastStatusAt = useRef<number | null>(null);
  const reported = deriveSubAgentStatus(childStatus);
  const localStatus = reported ?? requestedStop;
  // Either authority can say there is work to stop: the parent's progress
  // notes, or the child session's own status. A sub-agent session is never
  // prompted through this server, so its row would never offer Stop if it
  // waited for a run of its own. An approval gate is live work too — it is the
  // stop already made, or lost contact, that leaves nothing to ask for.
  const stopInFlight = localStatus === 'cancelling' || localStatus === 'cancelled' || localStatus === 'unconfirmed';
  const canStop = (note.status === 'running' || childStatus === 'running') && !stopInFlight && !!note.invocationId;
  useEffect(() => {
    if (!note.invocationId || ['done', 'failed', 'cancelled'].includes(note.status)) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    const refresh = async () => {
      try {
        const state = await sessionControl(note.agent, note.sessionId, { signal: controller.signal });
        if (disposed) return;
        setChildStatus(state.state.status);
        if (state.state.status === 'interrupting') lastStatusAt.current = Date.now();
      } catch (err) {
        if (!disposed) {
          handleSubAgentRefreshFailure({
            err,
            lastContact: lastStatusAt.current ?? requestedAt.current,
            clearChildStatus: () => setChildStatus(undefined),
            markUnconfirmed: () => setRequestedStop('unconfirmed'),
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
    setRequestedStop('cancelling');
    setError(undefined);
    try {
      // An idle session had nothing to stop; anything else is a durable
      // `Cancel` that the next status poll will confirm.
      const result = await cancel(note.agent, note.sessionId);
      if (result.outcome === 'idle') { setChildStatus(undefined); setRequestedStop(undefined); }
    } catch (error) {
      setRequestedStop('unconfirmed');
      if (!isAbortError(error)) {
        setError(String(error));
      }
    }
  };
        const statusLabel = STATUS_LABEL[localStatus ?? note.status];
        const displayedElapsedMs = elapsedMs(note, localStatus ? (stoppedAt ?? note.updatedAtMs) : nowMs);
        const sessionHref = `/agents/${encodeURIComponent(note.agent)}/sessions/${encodeURIComponent(note.sessionId)}`;
        return (
          <div
            className="aui-sub-agent-row"
            data-status={localStatus ?? note.status}
            data-elapsed-ms={Math.floor(displayedElapsedMs)}
          >
            <div className="aui-sub-agent-note">
              <span className="aui-sub-agent-identity">
                <span className="aui-sub-agent-identity-line">
                  <span className="aui-sub-agent-name">{note.agent}</span>
                  <span className="aui-sub-agent-session">{note.sessionId}</span>
                </span>
                {note.title?.trim() ? (
                  <span className="aui-sub-agent-title" title={note.title}>
                    {note.title}
                  </span>
                ) : null}
                <span className="aui-sub-agent-metrics">
                  <span>{formatElapsedMs(displayedElapsedMs)}</span>
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
            </div>
            <LinkButton
              href={sessionHref}
              onNavigate={() => onOpen(note.agent, note.sessionId)}
              className="aui-sub-agent-open"
              aria-label={`Open ${note.agent} sub-agent session ${note.sessionId} (${statusLabel.toLowerCase()})`}
              title={`Open ${note.agent} sub-agent session ${note.sessionId}`}
            >
              <OpenInNewIcon />
            </LinkButton>
            {canStop && <button type="button" className="aui-sub-agent-stop" aria-label={`Stop ${note.agent} sub-agent session ${note.sessionId}`} onClick={() => void stop()}>Stop</button>}
            {requestedStop === 'unconfirmed' && !reported && <button type="button" aria-label={`Retry stopping ${note.agent}`} onClick={() => void stop()}>Retry</button>}
            {error && <span role="alert">{error}</span>}
          </div>
        );

}
