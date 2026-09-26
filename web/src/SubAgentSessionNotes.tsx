import { Fragment, useContext, useEffect, useMemo, useRef, useState } from 'react';
import type { SubAgentNote } from './subAgentNotes';
import { SubAgentRow } from './SubAgentRow';
import { HarnxHttpAgent } from './ChatProvider';
import { SubAgentNotesContext } from './SubAgentNotesContext';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { AssistantRuntimeProvider } from '@assistant-ui/react';
import { RuntimeSessionSubscriber } from './RuntimeSessionSubscriber';

export interface SubAgentSessionNotesProps {
  notes: SubAgentNote[];
  onOpen: (agent: string, sessionId: string) => void;
}

function computeStartupDelay(startedAtMs?: number): number {
  if (!startedAtMs) return 0;
  return Math.max(0, 5000 - (Date.now() - startedAtMs));
}

function computeUsageElapsed(current: SubAgentNote): number {
  if (current.status === 'awaiting_approval') return current.elapsedMs;
  return current.startedAtMs
    ? Math.max(0, Date.now() - current.startedAtMs)
    : current.elapsedMs + Math.max(0, Date.now() - current.updatedAtMs);
}

function ChildMetricsSubscriber({ note, dispatch }: { note: SubAgentNote; dispatch: (event: unknown) => void }) {
  const noteRef = useRef(note);
  const dispatchRef = useRef(dispatch);
  const toolCallCountRef = useRef(note.toolCallCount || 0);
  const terminalTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const interruptedRef = useRef(false);

  useEffect(() => {
    noteRef.current = note;
    dispatchRef.current = dispatch;
    if (note.toolCallCount > toolCallCountRef.current) toolCallCountRef.current = note.toolCallCount;
  }, [note, dispatch]);

  useEffect(() => () => {
    if (terminalTimerRef.current !== null) clearTimeout(terminalTimerRef.current);
  }, []);

  const clearTerminalTimer = () => {
    if (terminalTimerRef.current !== null) {
      clearTimeout(terminalTimerRef.current);
      terminalTimerRef.current = null;
    }
  };

  const handleAwaitingApproval = () => {
    clearTerminalTimer();
    const current = noteRef.current;
    dispatchRef.current({
      type: 'CHILD_AWAITING_APPROVAL',
      invocationId: current.invocationId,
      toolCallId: current.toolCallId,
    });
  };

  const handleTurnInterrupted = () => {
    interruptedRef.current = true;
    clearTerminalTimer();
    const current = noteRef.current;
    dispatchRef.current({
      type: 'CHILD_TERMINAL',
      invocationId: current.invocationId,
      toolCallId: current.toolCallId,
      status: 'cancelled',
    });
  };

  const handleRunStarted = () => {
    clearTerminalTimer();
    interruptedRef.current = false;
    const current = noteRef.current;
    if (current.status === 'awaiting_approval') {
      dispatchRef.current({
        type: 'CHILD_RUNNING',
        invocationId: current.invocationId,
        toolCallId: current.toolCallId,
      });
    }
  };

  const scheduleTerminal = (status: 'cancelled' | 'failed' | 'done') => {
    const current = noteRef.current;
    const dispatchTerminal = () => {
      terminalTimerRef.current = null;
      dispatchRef.current({
        type: 'CHILD_TERMINAL',
        invocationId: current.invocationId,
        toolCallId: current.toolCallId,
        status,
      });
    };

    const delay = status === 'cancelled' ? 0 : computeStartupDelay(current.startedAtMs);
    if (delay > 0) {
      clearTerminalTimer();
      terminalTimerRef.current = setTimeout(dispatchTerminal, delay);
    } else {
      dispatchTerminal();
    }
  };

  const handleLifecycleTerminal = (event: any) => {
    const failed = event?.type === 'RUN_ERROR' || event?.result?.status === 'failed';
    const status = interruptedRef.current ? 'cancelled' : failed ? 'failed' : 'done';
    scheduleTerminal(status);
  };

  const handleUsage = (usage: any) => {
    const current = noteRef.current;
    dispatchRef.current({
      type: 'CUSTOM',
      name: 'sub_agent_progress',
      value: {
        invocation_id: current.invocationId,
        tool_call_id: current.toolCallId,
        agent: current.agent,
        session_id: current.sessionId,
        started_at: current.startedAtMs,
        elapsed_ms: computeUsageElapsed(current),
        tool_call_count: toolCallCountRef.current,
        status: current.status,
        usage: {
          input_tokens: usage.input,
          output_tokens: usage.output,
          cached_tokens: usage.cached ?? 0,
        },
      },
    });
  };

  const agent = useMemo(() => new HarnxHttpAgent({
    url: `/v1/agents/${encodeURIComponent(note.agent)}/sessions/${encodeURIComponent(note.sessionId)}`,
    onStatus: () => {},
    onRunFailed: () => {},
    isForeground: false,
    onHitlPendingApproval: handleAwaitingApproval,
    onTurnInterrupted: handleTurnInterrupted,
    onUsage: handleUsage,
    onToolSummary: () => { toolCallCountRef.current++; },
    onSubAgentEvent: (event: any) => {
      if (event?.type === 'RUN_STARTED') {
        handleRunStarted();
      } else if (event?.type === 'RUN_FINISHED' || event?.type === 'RUN_ERROR') {
        handleLifecycleTerminal(event);
      }
    },
  }), [note.agent, note.sessionId]);

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
      {notes.map((note) => (
        <Fragment key={note.id}>
          <SubAgentRow note={note} nowMs={nowMs} onOpen={onOpen} />
          {(note.status === 'running' || note.status === 'awaiting_approval') && (
            <ChildMetricsSubscriber note={note} dispatch={dispatch} />
          )}
        </Fragment>
      ))}
    </div>
  );
}
