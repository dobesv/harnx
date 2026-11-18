import { cancel } from './api';
import { isAbortError } from './httpClient';
import { formatElapsedMs } from './toolCallPresentation';
import { useState } from 'react';
import { LinkButton } from './LinkButton';
import { OpenInNewIcon } from './icons';
import type { SubAgentNote } from './subAgentNotes';
import type { SubAgentSessionNotesProps } from './SubAgentSessionNotes';
import { resolveChildAgentAddress } from './subAgentPendingInteraction';
import { SubAgentPendingControls } from './SubAgentPendingControls';

// The row renders note.status, the single event-driven source of truth: the
// child session's lifecycle, control events, and active pending interactions fold
// into the note, including the approval gate it is parked at and any interrupt that
// stopped it. Local state includes the optimistic stop state set on Stop-click
// (requestedStop) and inline pending interaction controls.
type RowStatus = SubAgentNote['status'];

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

function formatTokens(value: number) {
  return value.toLocaleString();
}

interface DerivedRowStatus {
  isTerminal: boolean;
  localStatus?: 'cancelling' | 'unconfirmed';
  status: RowStatus;
  canStop: boolean;
}

function deriveRowStatus(
  note: SubAgentNote,
  requestedStop?: 'cancelling' | 'unconfirmed',
): DerivedRowStatus {
  const isTerminal = ['done', 'failed', 'cancelled'].includes(note.status);
  const localStatus = isTerminal ? undefined : requestedStop;
  const status = localStatus ?? (note.pendingInteraction && !isTerminal ? 'awaiting_approval' : note.status);
  const stopInFlight =
    localStatus === 'cancelling' ||
    localStatus === 'unconfirmed' ||
    note.status === 'cancelling' ||
    note.status === 'cancelled';
  const canStop =
    (note.status === 'running' || note.status === 'awaiting_approval' || status === 'awaiting_approval') &&
    !stopInFlight &&
    !isTerminal &&
    Boolean(note.invocationId);
  return { isTerminal, localStatus, status, canStop };
}

function computeDisplayedElapsed(
  note: SubAgentNote,
  localStatus: 'cancelling' | 'unconfirmed' | undefined,
  stoppedAt: number | undefined,
  nowMs: number,
): number {
  const freezeTime = localStatus || note.status === 'awaiting_approval';
  return elapsedMs(note, freezeTime ? (stoppedAt ?? note.updatedAtMs) : nowMs);
}

async function executeStop(
  agent: string,
  sessionId: string,
  onIdle: () => void,
  onError: (msg?: string) => void,
) {
  try {
    const result = await cancel(agent, sessionId);
    if (result.outcome === 'idle') onIdle();
  } catch (error) {
    onError(isAbortError(error) ? undefined : String(error));
  }
}

function SubAgentMetricsView({
  note,
  displayedElapsedMs,
}: {
  note: SubAgentNote;
  displayedElapsedMs: number;
}) {
  return (
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
  );
}

export function SubAgentRow({
  note,
  nowMs,
  onOpen,
}: {
  note: SubAgentNote;
  nowMs: number;
  onOpen: SubAgentSessionNotesProps['onOpen'];
}) {
  const [requestedStop, setRequestedStop] = useState<'cancelling' | 'unconfirmed'>();
  const [error, setError] = useState<string>();
  const [stoppedAt, setStoppedAt] = useState<number>();

  const { isTerminal, localStatus, status, canStop } = deriveRowStatus(note, requestedStop);

  const stop = async () => {
    if (!note.invocationId) return;
    setStoppedAt((previous) => previous ?? Date.now());
    setRequestedStop('cancelling');
    setError(undefined);
    await executeStop(
      note.agent,
      note.sessionId,
      () => setRequestedStop(undefined),
      (msg) => {
        setRequestedStop('unconfirmed');
        if (msg) setError(msg);
      },
    );
  };

  const statusLabel = STATUS_LABEL[status];
  const displayedElapsedMs = computeDisplayedElapsed(note, localStatus, stoppedAt, nowMs);
  const pending = note.pendingInteraction;
  const targetAgent = pending?.agent ?? note.agent;
  const targetCluster = pending?.cluster;
  const targetSessionId = pending?.sessionId ?? note.sessionId;
  const childAddress = resolveChildAgentAddress(targetAgent, targetCluster);
  const sessionHref = `/agents/${encodeURIComponent(childAddress)}/sessions/${encodeURIComponent(targetSessionId)}`;

  return (
    <div
      className="aui-sub-agent-row"
      data-status={status}
      data-elapsed-ms={Math.floor(displayedElapsedMs)}
    >
      <div className="aui-sub-agent-note">
        <SubAgentMetricsView note={note} displayedElapsedMs={displayedElapsedMs} />
        <span className={`aui-sub-agent-status aui-sub-agent-status-${status}`}>
          <span className="aui-sub-agent-status-icon" aria-hidden="true" />
          {statusLabel}
        </span>
      </div>
      <LinkButton
        href={sessionHref}
        onNavigate={() => onOpen(childAddress, targetSessionId)}
        className="aui-sub-agent-open"
        aria-label={`Open ${targetAgent} sub-agent session ${targetSessionId} (${statusLabel.toLowerCase()})`}
        title={`Open ${targetAgent} sub-agent session ${targetSessionId}`}
      >
        <OpenInNewIcon />
      </LinkButton>
      {canStop && (
        <button
          type="button"
          className="aui-sub-agent-stop"
          aria-label={`Stop ${note.agent} sub-agent session ${note.sessionId}`}
          onClick={() => void stop()}
        >
          Stop
        </button>
      )}
      {requestedStop === 'unconfirmed' && !isTerminal && (
        <button
          type="button"
          aria-label={`Retry stopping ${note.agent}`}
          onClick={() => void stop()}
        >
          Retry
        </button>
      )}
      {error && <span role="alert">{error}</span>}
      <SubAgentPendingControls
        note={note}
        isTerminal={isTerminal}
        childAddress={childAddress}
        sessionHref={sessionHref}
        onOpen={onOpen}
      />
    </div>
  );
}
