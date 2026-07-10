import { useEffect, useRef, useContext, useState } from 'react';
import {
  ThreadPrimitive,
  MessagePrimitive,
  ComposerPrimitive,
  AttachmentPrimitive,
  useThread,
  useComposerRuntime,
  useMessage,
} from '@assistant-ui/react';
import { useAgUiInterrupts, useAgUiSubmitInterruptResponses } from '@assistant-ui/react-ag-ui';
import { ChatProvider } from './ChatProvider';
import { PendingContext } from './PendingContext';
import { cancel } from './api';
import type { Agent, SessionRef } from './types';
import { useAgentSessions } from './useAgentSessions';
import './chat.css';

// Activate a click-like handler from the keyboard (Enter / Space) so div-based
// "button" affordances (picker cards) are usable without a mouse.
function activateOnKey(e: React.KeyboardEvent, action: () => void) {
  if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault();
    action();
  }
}

const MyMessage = () => {
  const role = useMessage((state) => state.role);
  const [systemExpanded, setSystemExpanded] = useState(false);

  if (role === 'system') {
    return (
      <MessagePrimitive.Root className="aui-message aui-system-message">
        <details className="aui-system-message-details" open={systemExpanded} onToggle={(e) => setSystemExpanded((e.currentTarget as HTMLDetailsElement).open)}>
          <summary className="aui-system-message-summary" aria-label={systemExpanded ? 'Collapse system prompt' : 'Expand system prompt'}>
            {systemExpanded ? 'System prompt ▾' : 'System prompt ▸'}
          </summary>
          <div className="aui-message-content">
            <MessagePrimitive.Content components={{
              tools: { Fallback: (props: any) => (
              <div className="aui-tool-call">
                <div className="aui-tool-call-header">
                  <span className="aui-tool-call-icon">⚙️</span>
                  <span className="aui-tool-call-label">{props.toolName}</span>
                </div>
                <div className="aui-tool-call-body">
                  {JSON.stringify(props.args, null, 2)}
                </div>
              </div>
            ) }
            }} />
          </div>
        </details>
      </MessagePrimitive.Root>
    );
  }

  return (
    <MessagePrimitive.Root className="aui-message">
      <div className="aui-message-content">
        <MessagePrimitive.Content components={{
          tools: { Fallback: (props: any) => (
              <div className="aui-tool-call">
                <div className="aui-tool-call-header">
                  <span className="aui-tool-call-icon">⚙️</span>
                  <span className="aui-tool-call-label">{props.toolName}</span>
                </div>
                <div className="aui-tool-call-body">
                  {JSON.stringify(props.args, null, 2)}
                </div>
              </div>
            ) }
        }} />
      </div>
    </MessagePrimitive.Root>
  );
};

const CancelButton = ({ agentName, sessionId }: { agentName: string, sessionId: string }) => {
  const isRunning = useThread((s) => s.isRunning);
  if (!isRunning) return null;
  return (
    <button
      className="aui-cancel-button"
      onClick={() => cancel(agentName, sessionId).catch(console.error)}
    >
      Stop
    </button>
  );
};

const MyAttachment = () => (
  <AttachmentPrimitive.Root className="aui-attachment">
    <AttachmentPrimitive.unstable_Thumb className="aui-attachment-thumb" />
    <div className="aui-attachment-info">
      <span className="aui-attachment-name"><AttachmentPrimitive.Name /></span>
      <AttachmentPrimitive.Remove className="aui-attachment-remove">✖</AttachmentPrimitive.Remove>
    </div>
  </AttachmentPrimitive.Root>
);

const MyComposer = ({ agentName, sessionId }: { agentName: string, sessionId: string }) => {
  const { setErrorText } = useContext(PendingContext);
  const isRunning = useThread(s => s.isRunning);
  const composerRuntime = useComposerRuntime();

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setErrorText(null);

    const state = composerRuntime.getState();
    if (!state.text.trim() && state.attachments.length === 0) return;
    if (state.attachments.some((a: any) => a.status?.type !== 'complete')) return;

    composerRuntime.send();
  };

  return (
    <ComposerPrimitive.Root className="aui-composer" onSubmit={handleSubmit}>
      <div style={{ display: 'flex', width: '100%', alignItems: 'center' }}>
        <div className="aui-composer-attachments">
          <ComposerPrimitive.Attachments components={{ Attachment: MyAttachment }} />
        </div>
        <ComposerPrimitive.AddAttachment className="aui-composer-add-attachment">Attach</ComposerPrimitive.AddAttachment>
        <ComposerPrimitive.Input
          className="aui-composer-input"
          placeholder={isRunning ? 'Type a message (queued)...' : 'Type a message...'}
          render={<textarea />}
        />
        <button type="submit" className="aui-composer-send">{isRunning ? 'Queue' : 'Send'}</button>
        <CancelButton agentName={agentName} sessionId={sessionId} />
      </div>
    </ComposerPrimitive.Root>
  );
};

const RunStateMonitor = ({ onRunFinish }: { onRunFinish: () => void }) => {
  const isRunning = useThread(s => s.isRunning);
  const wasRunning = useRef(isRunning);
  useEffect(() => {
    if (wasRunning.current && !isRunning) {
      onRunFinish();
    }
    wasRunning.current = isRunning;
  }, [isRunning, onRunFinish]);
  return null;
};

const StatusIndicator = () => {
  const { statusText } = useContext(PendingContext);
  const isRunning = useThread(s => s.isRunning);
  if (!isRunning) return null;
  return (
    <div className="aui-status-indicator">
      <span className="aui-spinner"><span></span></span>
      <span className="aui-status-text">{statusText || 'Running...'}</span>
    </div>
  );
};

const SendErrorIndicator = () => {
  const { errorText } = useContext(PendingContext);
  if (!errorText) return null;
  return (
    <div className="aui-error" data-testid="send-error">
      {errorText}
    </div>
  );
};

const BatchInterruptUI = () => {
  const interrupts = useAgUiInterrupts();
  const submitResponses = useAgUiSubmitInterruptResponses();
  const [responses, setResponses] = useState<Record<string, 'resolved' | 'cancelled'>>({});

  if (!interrupts.length) return null;

  const handleSubmit = () => {
    const payload = interrupts.map(i => ({
      interruptId: i.id,
      status: responses[i.id] || 'cancelled'
    }));
    submitResponses(payload);
  };

  return (
    <div className="aui-interrupts-batch">
      <h4 className="aui-interrupts-title">Action Required: Approve Tool Calls</h4>
      {interrupts.map((interrupt) => (
        <div key={interrupt.id} className="aui-interrupt">
          <p className="aui-interrupt-tool-name">Tool: <strong>{interrupt.toolCallId || interrupt.reason}</strong></p>
          {interrupt.message && <pre className="aui-interrupt-message">{interrupt.message}</pre>}
          <div className="aui-interrupt-actions">
            <label>
              <input type="radio" name={`action-${interrupt.id}`} checked={responses[interrupt.id] === 'resolved'} onChange={() => setResponses((prev: Record<string, 'resolved' | 'cancelled'>) => ({ ...prev, [interrupt.id]: 'resolved' }))} /> Approve
            </label>
            <label>
              <input type="radio" name={`action-${interrupt.id}`} checked={responses[interrupt.id] === 'cancelled'} onChange={() => setResponses((prev: Record<string, 'resolved' | 'cancelled'>) => ({ ...prev, [interrupt.id]: 'cancelled' }))} /> Deny
            </label>
          </div>
        </div>
      ))}
      <button
        disabled={Object.keys(responses).length !== interrupts.length}
        onClick={handleSubmit}
        className="aui-interrupt-submit"
      >
        Submit Decisions
      </button>
    </div>
  );
};

const MyThread = ({ agentName, sessionId, onRunFinish }: { agentName: string, sessionId: string, onRunFinish: () => void }) => {
  const isEmpty = useThread(s => s.messages.length === 0);

  return (
    <ThreadPrimitive.Root className={`aui-thread ${isEmpty ? 'aui-thread-empty' : ''}`}>
      <RunStateMonitor onRunFinish={onRunFinish} />
      <StatusIndicator />

      {!isEmpty && (
        <ThreadPrimitive.Viewport className="aui-thread-viewport">
          <ThreadPrimitive.Messages components={{ Message: MyMessage }} />
        </ThreadPrimitive.Viewport>
      )}

      <div className="aui-thread-bottom">
        <BatchInterruptUI />
        <SendErrorIndicator />
        <div className="aui-composer-container">
          <MyComposer agentName={agentName} sessionId={sessionId} />
        </div>
      </div>
    </ThreadPrimitive.Root>
  );
};

const AgentPicker = ({
  agents,
  agentsError,
  onSelect
}: {
  agents: Agent[];
  agentsError: string | null;
  onSelect: (agent: string) => void;
}) => (
  <div className="picker-container">
    <h2>Select an Agent</h2>
    {agentsError ? (
      <div role="alert" className="aui-error" data-testid="agents-error">{agentsError}</div>
    ) : (
      <div className="grid-list">
        {agents.map(a => (
          <div
            key={a.name}
            className="grid-item"
            role="button"
            tabIndex={0}
            onClick={() => onSelect(a.name)}
            onKeyDown={(e) => activateOnKey(e, () => onSelect(a.name))}
          >
            <h3>{a.name}</h3>
            {a.description && <p>{a.description}</p>}
          </div>
        ))}
      </div>
    )}
  </div>
);

const SessionPicker = ({
  agentName,
  sessions,
  sessionsError,
  onSelect,
  onNewChat,
  onBack
}: {
  agentName: string;
  sessions: SessionRef[];
  sessionsError: string | null;
  onSelect: (id: string) => void;
  onNewChat: () => void;
  onBack: () => void;
}) => (
  <div className="picker-container">
    <button className="back-button" onClick={onBack}>&larr; Back to agents</button>
    <h2>Sessions for {agentName}</h2>
    <div className="actions-bar">
      <button className="new-chat-button" onClick={onNewChat}>New Chat</button>
    </div>
    {sessionsError ? (
      <div role="alert" className="aui-error" data-testid="sessions-error">{sessionsError}</div>
    ) : (
      <div className="grid-list sessions-grid">
        {sessions.length === 0 ? (
          <p className="no-sessions-msg">No existing sessions found.</p>
        ) : (
          sessions.map(s => (
            <div
              key={s.session_id}
              className="grid-item session-item"
              role="button"
              tabIndex={0}
              onClick={() => onSelect(s.session_id)}
              onKeyDown={(e) => activateOnKey(e, () => onSelect(s.session_id))}
            >
              <h3>{s.session_id}</h3>
              {s.updated_at && <p>Updated: {new Date(s.updated_at).toLocaleString()}</p>}
            </div>
          ))
        )}
      </div>
    )}
  </div>
);

const TopNav = ({
  agentName,
  sessionId,
  onSwitchAgent,
  onSwitchSession
}: {
  agentName: string;
  sessionId: string;
  onSwitchAgent: () => void;
  onSwitchSession: () => void;
}) => (
  <div className="top-nav">
    <div className="top-nav-brand">Harnx UI</div>
    <div className="top-nav-controls">
      <div className="top-nav-item">
        <span className="top-nav-label">Agent:</span>
        <span className="top-nav-value">{agentName}</span>
        <button onClick={onSwitchAgent}>Switch agent</button>
      </div>
      <div className="top-nav-item">
        <span className="top-nav-label">Session:</span>
        <span className="top-nav-value">{sessionId}</span>
        <button onClick={onSwitchSession}>Switch session</button>
      </div>
    </div>
  </div>
);

export default function App() {
  const {
    agents,
    agentsError,
    sessions,
    sessionsError,
    selectedAgent,
    selectedSessionId,
    refreshSessions,
    selectAgent,
    selectSession,
    newChat,
    clearAgent,
    clearSession,
    isFreshSession
  } = useAgentSessions();

  return (
    <div className="app-container">
      {!selectedAgent ? (
        <AgentPicker
          agents={agents}
          agentsError={agentsError}
          onSelect={selectAgent}
        />
      ) : !selectedSessionId ? (
        <SessionPicker
          agentName={selectedAgent}
          sessions={sessions}
          sessionsError={sessionsError}
          onSelect={selectSession}
          onNewChat={newChat}
          onBack={clearAgent}
        />
      ) : (
        <div className="chat-layout">
          <TopNav
            agentName={selectedAgent}
            sessionId={selectedSessionId}
            onSwitchAgent={clearAgent}
            onSwitchSession={clearSession}
          />
          <div className="chat-main">
            <ChatProvider agentName={selectedAgent} sessionId={selectedSessionId} isFreshSession={isFreshSession}>
              <MyThread agentName={selectedAgent} sessionId={selectedSessionId} onRunFinish={refreshSessions} />
            </ChatProvider>
          </div>
        </div>
      )}
    </div>
  );
}