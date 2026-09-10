import { useEffect, useRef, useContext, useState, useCallback, useMemo } from 'react';
import {
  ThreadPrimitive,
  MessagePrimitive,
  ComposerPrimitive,
  AttachmentPrimitive,
  type Attachment,
  useAui,
  useAuiState,
} from '@assistant-ui/react';
import { MarkdownTextPrimitive } from '@assistant-ui/react-markdown';
import { makeLightAsyncSyntaxHighlighter } from '@assistant-ui/react-syntax-highlighter';
import remarkGfm from 'remark-gfm';

const SyntaxHighlighter = makeLightAsyncSyntaxHighlighter({ useInlineStyles: false });
import { ToolCallCard } from './ToolCallCard';
import { useAgUiInterrupts } from '@assistant-ui/react-ag-ui';
import { ChatProvider, attachmentToMessageParts } from './ChatProvider';
import { PendingContext } from './PendingContext';
import { UsageContext, type UsageData } from './UsageContext';
import { SubAgentNotesContext } from './SubAgentNotesContext';
import { SubAgentSessionNotes } from './SubAgentSessionNotes';
import { cancel, sendPrompt, uploadAttachment, submitHitlDecision } from './api';
import type { Agent, SessionRef } from './types';
import { useAgentSessions } from './useAgentSessions';
import { AttachIcon, SendIcon } from './icons';
import { AgentDropdown, SessionDropdown, AgentSessionMenu } from './composer/AgentSessionMenu';
import './chat.css';

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
  const messageId = useAuiState((s) => s.message.id);
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
  const { notes, openSession } = useContext(SubAgentNotesContext);
  const messageNotes = role === 'assistant'
    ? notes.filter((note) => note.parentMessageId === messageId)
    : [];

  if (isEmpty && messageNotes.length === 0) return null;

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
      <SubAgentSessionNotes notes={messageNotes} onOpen={openSession} />
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

export const MyComposer = ({
  agentName,
  sessionId,
  isFreshSession,
  markSessionNotFresh,
  onSwitchAgent,
  onSwitchSession,
  switchAgentHref,
  switchSessionHref,
}: {
  agentName: string;
  sessionId: string;
  isFreshSession: boolean;
  markSessionNotFresh: (sessionId: string) => void;
  onSwitchAgent: () => void;
  onSwitchSession: () => void;
  switchAgentHref: string;
  switchSessionHref: string;
}) => {
  const { setErrorText } = useContext(PendingContext);
  const composerRuntime = useAui().composer;
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const [isSending, setIsSending] = useState(false);
  const userMessageCount = useAuiState(s => s.thread.messages.filter(m => m.role === 'user').length);
  const prevUserMessageCount = useRef(userMessageCount);

  useEffect(() => {
    if (isSending && userMessageCount > prevUserMessageCount.current) {
      setIsSending(false);
    }
  }, [isSending, userMessageCount]);

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
    if (isSending) return;
    setErrorText(null);

    const state = composerRuntime.getState();
    const text = state.text.trim();
    if (!text && state.attachments.length === 0) return;

    if (isFreshSession) {
      // Fresh session: streaming POST via runAgent (composerRuntime.send).
      // Only the first message follows this path; subsequent messages go out-of-band
      // via session/prompt while RuntimeSessionSubscriber passively follows the stream.
      // See design note (#1761 Option A) for why we must NOT optimistically append
      // the user row to the thread — RuntimeSessionSubscriber.startRun({parentId})
      // would resend it and the server would treat an unknown client id as a new turn.
      if (state.attachments.some((a: Attachment) => a.status?.type !== 'complete')) return;
      composerRuntime.send();
      markSessionNotFresh(sessionId);
      collapseTextarea();
    } else {
      // Existing session: JSON-RPC session/prompt (no runAgent).
      // Attaches to the existing stream via RuntimeSessionSubscriber.startRun({parentId}),
      // which hydrates the server-authored user row without re-execution.
      setIsSending(true);
      prevUserMessageCount.current = userMessageCount;
      const savedText = text;
      const savedAttachments = state.attachments;

      resetComposerInput();

      // assistant-ui (0.15.18) only uploads attachments inside composerRuntime.send().
      // addAttachment() merely marks them as 'running' — the adapter.send() call that
      // produces the CID happens during send(). Since this out-of-band path deliberately
      // avoids composerRuntime.send(), we must upload fresh attachments ourselves.
      // Attachments retained from a previous send already have CIDs in .content.
      const doSend = async () => {
        const attachmentRefs: string[] = [];
        for (const att of savedAttachments) {
          const parts = attachmentToMessageParts(att);
          let hasCid = false;
          for (const p of parts) {
            if (p.type === 'image' && typeof p.image === 'string') {
              attachmentRefs.push(p.image);
              hasCid = true;
            } else if (p.type === 'file' && typeof p.data === 'string') {
              attachmentRefs.push(p.data);
              hasCid = true;
            }
          }

          if (!hasCid && att.file) {
            const refs = await uploadAttachment(agentName, sessionId, att.file as File);
            attachmentRefs.push(...refs);
          }
        }

        await sendPrompt(agentName, sessionId, { text, attachmentRefs });
      };

      doSend().catch(err => {
        console.error('Failed to send prompt or upload attachments out of band', err);
        setErrorText(err instanceof Error ? err.message : String(err));
        // Restore input
        composerRuntime.setText(savedText);
        savedAttachments.forEach((att: Attachment) => {
          if (att.file) void composerRuntime.addAttachment(att.file);
        });
        setIsSending(false);
        requestAnimationFrame(() => {
          textareaRef.current?.focus();
          resizeTextarea(textareaRef.current);
        });
      });
    }
  };

  
  const placeholder = 'Type a message...';
  const menuProps = { agentName, sessionId, switchAgentHref, switchSessionHref, onSwitchAgent, onSwitchSession };
  const sendLabel = 'Send';

  return (
    <ComposerPrimitive.Root className="aui-composer" onSubmit={handleSubmit}>
      <div className="aui-composer-attachments">
        <ComposerPrimitive.Attachments components={{ Attachment: MyAttachment }} />
      </div>
      <ComposerPrimitive.Input
        className="aui-composer-input"
        placeholder={placeholder}
        render={<textarea disabled={isSending} ref={setTextareaRef} rows={1} onInput={(e) => resizeTextarea(e.currentTarget)} />}
      />
      <div className="aui-composer-controls">
        <ComposerPrimitive.AddAttachment disabled={isSending} className="aui-composer-add-attachment aui-composer-icon-btn" aria-label="Attach file" title="Attach file">
          <AttachIcon />
          <span className="aui-visually-hidden">Attach file</span>
        </ComposerPrimitive.AddAttachment>
        
        <div className="aui-composer-controls-desktop">
          <AgentDropdown {...menuProps} />
          <SessionDropdown {...menuProps} />
        </div>
        <div className="aui-composer-controls-mobile">
          <AgentSessionMenu {...menuProps} />
        </div>
        
        <button disabled={isSending} type="submit" className="aui-composer-send aui-composer-icon-btn" aria-label={sendLabel} title={sendLabel}>
          {isSending ? <span className="aui-spinner"><span></span></span> : <SendIcon />}
          <span className="aui-visually-hidden">{sendLabel}</span>
        </button>
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
    <div role="alert" className="aui-error" data-testid="send-error">
      {errorText}
    </div>
  );
};

export const BatchInterruptUI = ({ agentName, sessionId }: { agentName: string; sessionId: string }) => {
  const interrupts = useAgUiInterrupts();
  const { setErrorText, hydratedApprovals, removeHydratedApproval } = useContext(PendingContext);
  const [submitting, setSubmitting] = useState(false);
  const [note, setNote] = useState('');
  const [resolvedToolCallIds, setResolvedToolCallIds] = useState<Set<string>>(() => new Set());

  const pendingItems = useMemo(() => {
    const items: Array<{ toolCallId: string; summary: string }> = [];
    const seen = new Set<string>();

    for (const a of hydratedApprovals) {
      if (!resolvedToolCallIds.has(a.toolCallId)) {
        items.push(a);
        seen.add(a.toolCallId);
      }
    }

    for (const i of interrupts) {
      if (i.toolCallId && !seen.has(i.toolCallId) && !resolvedToolCallIds.has(i.toolCallId)) {
        items.push({ toolCallId: i.toolCallId, summary: i.message || i.reason || i.toolCallId });
        seen.add(i.toolCallId);
      }
    }

    return items;
  }, [hydratedApprovals, interrupts, resolvedToolCallIds]);

  if (pendingItems.length === 0) return null;

  const currentItem = pendingItems[0];

  const handleDecision = async (approved: boolean) => {
    setErrorText(null);
    setSubmitting(true);
    try {
      await submitHitlDecision(agentName, sessionId, {
        toolCallId: currentItem.toolCallId,
        approved,
        note: note.trim() || undefined
      });
      removeHydratedApproval(currentItem.toolCallId);
      setResolvedToolCallIds((prev) => new Set(prev).add(currentItem.toolCallId));
      setNote('');
    } catch (err) {
      console.error('Failed to submit decision', err);
      setErrorText(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="aui-interrupts-batch">
      <h4 className="aui-interrupts-title">Action Required: Approve Tool Call</h4>
      <div className="aui-interrupt" data-testid="hydrated-pending-approval">
        <p className="aui-interrupt-tool-name">
          Tool: <strong>{currentItem.summary}</strong>
        </p>
        
        <div style={{ marginTop: '10px' }}>
          <label htmlFor="hitl-optional-note" style={{ display: 'block', fontSize: '0.9em', marginBottom: '4px' }}>
            Optional Note:
          </label>
          <input
            id="hitl-optional-note"
            type="text"
            value={note}
            onChange={(e) => setNote(e.target.value)}
            disabled={submitting}
            placeholder="Reason for approval or denial..."
            style={{ width: '100%', padding: '6px', boxSizing: 'border-box' }}
          />
        </div>

        <div className="aui-interrupt-actions" style={{ marginTop: '10px', display: 'flex', gap: '10px' }}>
          <button
            disabled={submitting}
            onClick={() => handleDecision(true)}
            className="aui-interrupt-submit"
            style={{ backgroundColor: '#2e7d32', color: 'white', padding: '6px 12px', border: 'none', borderRadius: '4px', cursor: 'pointer' }}
          >
            Approve
          </button>
          <button
            disabled={submitting}
            onClick={() => handleDecision(false)}
            className="aui-interrupt-submit"
            style={{ backgroundColor: '#c62828', color: 'white', padding: '6px 12px', border: 'none', borderRadius: '4px', cursor: 'pointer' }}
          >
            Deny
          </button>
        </div>
        
        {pendingItems.length > 1 && (
          <p className="aui-interrupt-note" style={{ fontSize: '0.85em', color: '#666', marginTop: '10px' }}>
            {pendingItems.length - 1} more tool call{pendingItems.length - 1 !== 1 ? 's' : ''} awaiting approval
          </p>
        )}
      </div>
    </div>
  );
};

const MyThread = ({ agentName, sessionId, isFreshSession, markSessionNotFresh, onRunFinish, onSwitchAgent, onSwitchSession, switchAgentHref, switchSessionHref }: { agentName: string, sessionId: string, isFreshSession: boolean, markSessionNotFresh: (sessionId: string) => void, onRunFinish: () => void, onSwitchAgent: () => void, onSwitchSession: () => void, switchAgentHref: string, switchSessionHref: string }) => {
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
        <BatchInterruptUI agentName={agentName} sessionId={sessionId} />
        <SendErrorIndicator />
        <div className="aui-composer-container">
          <MyComposer
            agentName={agentName}
            sessionId={sessionId}
            isFreshSession={isFreshSession}
            markSessionNotFresh={markSessionNotFresh}
            onSwitchAgent={onSwitchAgent}
            onSwitchSession={onSwitchSession}
            switchAgentHref={switchAgentHref}
            switchSessionHref={switchSessionHref}
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
    isFreshSession,
    markSessionNotFresh,
    navigateSession,
  } = useAgentSessions();

  const handleHandoff = useCallback((agent: string, sessionId: string) => {
    navigateSession(agent, sessionId);
  }, [navigateSession]);

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
          <div className="chat-main">
            <ChatProvider
              key={`${selectedAgent}:${selectedSessionId}`}
              agentName={selectedAgent} 
              sessionId={selectedSessionId} 
              isFreshSession={isFreshSession}
              onHandoff={handleHandoff}
              onOpenSubAgent={navigateSession}
            >
              <MyThread
                agentName={selectedAgent}
                sessionId={selectedSessionId}
                isFreshSession={isFreshSession}
                markSessionNotFresh={markSessionNotFresh}
                onRunFinish={refreshSessions}
                onSwitchAgent={clearAgent}
                onSwitchSession={clearSession}
                switchAgentHref="/"
                switchSessionHref={`/agents/${encodeURIComponent(selectedAgent)}`}
              />
            </ChatProvider>
          </div>
        </div>
      )}
    </div>
  );
}
