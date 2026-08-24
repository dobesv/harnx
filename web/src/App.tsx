import { useEffect, useRef, useContext, useState, useCallback } from 'react';
import {
  ThreadPrimitive,
  MessagePrimitive,
  ComposerPrimitive,
  AttachmentPrimitive,
  useAui,
  useAuiState,
} from '@assistant-ui/react';
import { MarkdownTextPrimitive } from '@assistant-ui/react-markdown';
import { makeLightAsyncSyntaxHighlighter } from '@assistant-ui/react-syntax-highlighter';
import remarkGfm from 'remark-gfm';

const SyntaxHighlighter = makeLightAsyncSyntaxHighlighter({ useInlineStyles: false });
import { ToolCallCard } from './ToolCallCard';
import { useAgUiInterrupts, useAgUiSubmitInterruptResponses } from '@assistant-ui/react-ag-ui';
import { ChatProvider } from './ChatProvider';
import { PendingContext } from './PendingContext';
import { UsageContext, type UsageData } from './UsageContext';
import { cancel } from './api';
import type { Agent, SessionRef } from './types';
import { useAgentSessions } from './useAgentSessions';
import './chat.css';

interface QueuedMessage {
  text: string;
}

// Activate a click-like handler from keyboard (Enter / Space) so div-based
// "button" affordances (picker cards) are usable without mouse.
function activateOnKey(e: React.KeyboardEvent, action: () => void) {
  if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault();
    action();
  }
}

function formatTokenCount(value: number | undefined) {
  if (value === undefined) return '';
  return value.toLocaleString();
}

const MessageContent = () => (
  <MessagePrimitive.Content components={{
    Text: () => (
      <MarkdownTextPrimitive
        remarkPlugins={[remarkGfm]}
        components={{
          SyntaxHighlighter,
          table: ({ node: _node, ...props }: any) => (
            <div className="overflow-x-auto">
              <table {...props} />
            </div>
          )
        }}
      />
    ),
    tools: { Fallback: ToolCallCard }
  }} />
);

const MyMessage = () => {
  const role = useAuiState((s) => s.message.role);
  // An assistant message with no parts still gets .aui-message padding, so it
  // shows up as a blank gap in the transcript. The promptless subscribe that
  // hydrates a session leaves one behind: assistant-ui creates the message
  // when the run starts, and a run carrying only a transcript snapshot never
  // puts content in it. Streaming replies are unaffected -- they render as
  // soon as the first part arrives.
  const isEmpty = useAuiState(
    (s) => s.message.role === 'assistant' && s.message.content.length === 0,
  );
  const [systemExpanded, setSystemExpanded] = useState(false);

  if (isEmpty) return null;

  if (role === 'system') {
    return (
      <MessagePrimitive.Root className="aui-message aui-system-message">
        <details className="aui-system-message-details" open={systemExpanded} onToggle={(e) => setSystemExpanded((e.currentTarget as HTMLDetailsElement).open)}>
          <summary className="aui-system-message-summary" aria-label={systemExpanded ? 'Collapse system prompt' : 'Expand system prompt'}>
            {systemExpanded ? 'System prompt ▾' : 'System prompt ▸'}
          </summary>
          <div className="aui-message-content">
            <MessageContent />
          </div>
        </details>
      </MessagePrimitive.Root>
    );
  }

  const roleClass = role === 'user' ? 'aui-user-message' : 'aui-assistant-message';
  return (
    <MessagePrimitive.Root className={`aui-message ${roleClass}`}>
      <div className="aui-message-content">
        <MessageContent />
      </div>
    </MessagePrimitive.Root>
  );
};

const CancelButton = ({ agentName, sessionId }: { agentName: string, sessionId: string }) => {
  const isRunning = useAuiState((s) => s.thread.isRunning);
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

const MyComposer = ({
  agentName,
  sessionId,
}: {
  agentName: string;
  sessionId: string;
}) => {
  const { setErrorText } = useContext(PendingContext);
  const isRunning = useAuiState(s => s.thread.isRunning);
  const composerRuntime = useAui().composer;
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const [queuedMessage, setQueuedMessage] = useState<QueuedMessage | null>(null);

  // Monitor run state to flush queued message
  const wasRunning = useRef(isRunning);
  useEffect(() => {
    if (wasRunning.current && !isRunning) {
      if (queuedMessage?.text.trim()) {
        try {
          composerRuntime.setText(queuedMessage.text);
          composerRuntime.send();
          setQueuedMessage(null);
        } catch (err) {
          console.error('Failed to send queued message', err);
        }
      }
    }
    wasRunning.current = isRunning;
  }, [isRunning, composerRuntime, queuedMessage]);

  const resizeTextarea = useCallback((el: HTMLTextAreaElement | null) => {
    if (!el) return;
    el.style.height = 'auto';
    const maxHeight = parseFloat(getComputedStyle(el).maxHeight) || Infinity;
    const contentHeight = el.scrollHeight;
    if (contentHeight > maxHeight) {
      el.style.height = `${maxHeight}px`;
      el.style.overflowY = 'auto';
    } else {
      el.style.height = `${contentHeight}px`;
      el.style.overflowY = 'hidden';
    }
  }, []);

  const setTextareaRef = useCallback((el: HTMLTextAreaElement | null) => {
    textareaRef.current = el;
    resizeTextarea(el);
  }, [resizeTextarea]);

  // Collapse the textarea back to its single-line, no-scrollbar state.
  // Deferred to the next frame so it runs after React has cleared the input
  // value in the DOM; otherwise height/scrollHeight would be measured against
  // the stale (pre-clear) content and the textarea would stay expanded.
  const collapseTextarea = useCallback(() => {
    requestAnimationFrame(() => {
      const textarea = textareaRef.current;
      if (!textarea) return;
      textarea.style.height = 'auto';
      textarea.style.overflowY = 'hidden';
    });
  }, []);

  const resetComposerInput = useCallback(() => {
    composerRuntime.setText('');
    void composerRuntime.clearAttachments();
    collapseTextarea();
  }, [composerRuntime, collapseTextarea]);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setErrorText(null);

    const state = composerRuntime.getState();
    const text = state.text.trim();
    if (!text && state.attachments.length === 0) return;
    if (state.attachments.some((a: any) => a.status?.type !== 'complete')) return;

    if (isRunning) {
      setQueuedMessage((current) => {
        if (!current) return { text };
        return { text: current.text.trim() ? `${current.text}\n${text}` : text };
      });
      resetComposerInput();
      return;
    }

    composerRuntime.send();
    collapseTextarea();
  };

  const queueCountLabel = queuedMessage ? '1 message queued' : null;
  const placeholder = queuedMessage
    ? 'Current run in progress. Next message queued.'
    : isRunning
      ? 'Type a message to queue after this run...'
      : 'Type a message...';

  return (
    <ComposerPrimitive.Root className="aui-composer" onSubmit={handleSubmit}>
      {queueCountLabel ? <div className="aui-composer-queue-hint">{queueCountLabel}</div> : null}
      <div className="aui-composer-row">
        <div className="aui-composer-attachments">
          <ComposerPrimitive.Attachments components={{ Attachment: MyAttachment }} />
        </div>
        <ComposerPrimitive.AddAttachment className="aui-composer-add-attachment">Attach</ComposerPrimitive.AddAttachment>
        <ComposerPrimitive.Input
          className="aui-composer-input"
          placeholder={placeholder}
          render={<textarea ref={setTextareaRef} rows={1} onInput={(e) => resizeTextarea(e.currentTarget)} />}
        />
        <button type="submit" className="aui-composer-send">{queuedMessage ? 'Queued' : isRunning ? 'Queue' : 'Send'}</button>
        <CancelButton agentName={agentName} sessionId={sessionId} />
      </div>
    </ComposerPrimitive.Root>
  );
};

const RunStateMonitor = ({ onRunFinish }: { onRunFinish: () => void }) => {
  const isRunning = useAuiState(s => s.thread.isRunning);
  const wasRunning = useRef(isRunning);
  useEffect(() => {
    if (wasRunning.current && !isRunning) {
      onRunFinish();
    }
    wasRunning.current = isRunning;
  }, [isRunning, onRunFinish]);
  return null;
};

const StatusIndicator = ({ isRunning, statusText }: { isRunning: boolean, statusText: string | null }) => (
  <div className="aui-status-left">
    {isRunning ? (
      <span className="aui-spinner"><span></span></span>
    ) : (
      <span className="aui-idle-dot"></span>
    )}
    <span className="aui-status-text">{statusText || (isRunning ? 'Running...' : 'Idle')}</span>
  </div>
);

const UsageItem = ({ icon, label, value }: { icon: string, label: string, value: string }) => (
  <span className="aui-status-usage-item" title={label} aria-label={`${label}: ${value}`}>
    <span className="aui-status-usage-icon" aria-hidden="true">{icon}</span>
    <span>{value}</span>
  </span>
);

const UsageIndicator = ({ usage }: { usage: UsageData }) => {
  const roundedContextPercent = usage.context_percent !== undefined ? Math.round(usage.context_percent) : undefined;

  return (
    <div className="aui-status-usage">
      <UsageItem icon="↘" label="Input tokens" value={formatTokenCount(usage.input)} />
      <UsageItem icon="↗" label="Output tokens" value={formatTokenCount(usage.output)} />
      {usage.cached ? <UsageItem icon="◌" label="Cached tokens" value={formatTokenCount(usage.cached)} /> : null}
      {usage.context_tokens !== undefined && (
        <UsageItem
          icon="◔"
          label="Context usage"
          value={`${formatTokenCount(usage.context_tokens)}${roundedContextPercent !== undefined ? ` (${roundedContextPercent}%)` : ''}`}
        />
      )}
    </div>
  );
};

const StatusBar = () => {
  const { statusText } = useContext(PendingContext);
  const { usage } = useContext(UsageContext);
  const isRunning = useAuiState(s => s.thread.isRunning);

  if (!isRunning && !usage && !statusText) return null;

  return (
    <div className="aui-status-bar">
      <StatusIndicator isRunning={isRunning} statusText={statusText} />
      {usage && <UsageIndicator usage={usage} />}
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
  const isEmpty = useAuiState(s => s.thread.messages.length === 0);

  return (
    <ThreadPrimitive.Root className={`aui-thread ${isEmpty ? 'aui-thread-empty' : ''}`}>
      <RunStateMonitor onRunFinish={onRunFinish} />

      {!isEmpty && (
        <ThreadPrimitive.Viewport className="aui-thread-viewport">
          <ThreadPrimitive.Messages components={{ Message: MyMessage }} />
        </ThreadPrimitive.Viewport>
      )}

      <div className="aui-thread-bottom">
        <StatusBar />
        <BatchInterruptUI />
        <SendErrorIndicator />
        <div className="aui-composer-container">
          <MyComposer
            agentName={agentName}
            sessionId={sessionId}
          />
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
  sessionsLoading,
  onRetry,
  onSelect,
  onNewChat,
  onBack
}: {
  agentName: string;
  sessions: SessionRef[];
  sessionsError: string | null;
  sessionsLoading: boolean;
  onRetry: () => void;
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
    {sessionsLoading ? (
      <p className="sessions-loading" role="status">Loading sessions…</p>
    ) : sessionsError ? (
      <div role="alert" className="aui-error" data-testid="sessions-error">
        <span>{sessionsError}</span>
        <button type="button" onClick={onRetry}>Retry</button>
      </div>
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

const BreadcrumbButton = ({ children, onClick }: { children: React.ReactNode, onClick: () => void }) => (
  <button type="button" className="top-nav-crumb-button" onClick={onClick}>
    {children}
  </button>
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
  <div className="top-nav" aria-label="Breadcrumb">
    <div className="top-nav-breadcrumbs">
      <BreadcrumbButton onClick={onSwitchAgent}>harnx</BreadcrumbButton>
      {agentName ? (
        <>
          <span className="top-nav-separator" aria-hidden="true">›</span>
          <BreadcrumbButton onClick={onSwitchSession}>{agentName}</BreadcrumbButton>
        </>
      ) : null}
      {sessionId ? (
        <>
          <span className="top-nav-separator" aria-hidden="true">›</span>
          <span className="top-nav-crumb-active" aria-current="page">{sessionId}</span>
        </>
      ) : null}
    </div>
  </div>
);

export default function App() {
  const {
    agents,
    agentsError,
    sessions,
    sessionsError,
    sessionsLoading,
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

  const handleHandoff = useCallback((agent: string, sessionId: string | null) => {
    if (agent) selectAgent(agent);
    if (sessionId) selectSession(sessionId);
  }, [selectAgent, selectSession]);

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
          sessionsLoading={sessionsLoading}
          onRetry={refreshSessions}
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
            <ChatProvider 
              agentName={selectedAgent} 
              sessionId={selectedSessionId} 
              isFreshSession={isFreshSession}
              onHandoff={handleHandoff}
            >
              <MyThread agentName={selectedAgent} sessionId={selectedSessionId} onRunFinish={refreshSessions} />
            </ChatProvider>
          </div>
        </div>
      )}
    </div>
  );
}
