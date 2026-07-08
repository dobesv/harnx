import { useEffect, useRef, useContext, useState } from 'react';
import {
  ThreadPrimitive,
  MessagePrimitive,
  ComposerPrimitive,
  useThread,
  useComposer,
  useComposerRuntime
} from '@assistant-ui/react';
import { useAgUiInterrupts, useAgUiSubmitInterruptResponses } from '@assistant-ui/react-ag-ui';
import { ChatProvider } from './ChatProvider';
import { PendingContext } from './PendingContext';
import { cancel, prompt } from './api';
import type { Agent, SessionRef } from './types';
import { useAgentSessions } from './useAgentSessions';
import './chat.css';

const MyMessage = () => {
  return (
    <MessagePrimitive.Root className="aui-message">
      <MessagePrimitive.If user>
        <div className="aui-message-role">You</div>
      </MessagePrimitive.If>
      <MessagePrimitive.If assistant>
        <div className="aui-message-role">AI</div>
      </MessagePrimitive.If>
      <div className="aui-message-content">
        <MessagePrimitive.Content />
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
      onClick={() => cancel(agentName, sessionId).catch(e => console.error("Cancel failed", e))}
    >
      Cancel Run
    </button>
  );
};

const MyComposer = ({ agentName, sessionId }: { agentName: string, sessionId: string }) => {
  const { pendingText, setPendingText, setErrorText } = useContext(PendingContext);
  const isRunning = useThread(s => s.isRunning);
  const composerRuntime = useComposerRuntime();
  const text = useComposer(s => s.text);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (isRunning) {
      const state = composerRuntime.getState();
      if (!state.text.trim() && state.attachments.length === 0) return;
      if (state.attachments.some((a: any) => a.status !== 'complete')) return;

      setErrorText(null);
      try {
        const attachment_refs = state.attachments.map((a: any) => (a as any).url).filter(Boolean);
        await prompt(agentName, sessionId, state.text, attachment_refs);
        setPendingText(state.text || "Attached file");
        composerRuntime.reset();
      } catch (err: any) {
        console.error("Failed to enqueue message", err);
        setErrorText(err instanceof Error ? err.message : 'Failed to send message');
      }
    }
  };

  return (
    <ComposerPrimitive.Root className="aui-composer">
      {isRunning && pendingText ? (
        <div className="aui-composer-pending">Pending: {pendingText}</div>
      ) : isRunning ? (
        <form onSubmit={handleSubmit} style={{ display: 'flex', width: '100%', alignItems: 'center' }}>
          <div className="aui-composer-attachments">
             <ComposerPrimitive.Attachments components={{}} />
          </div>
          <ComposerPrimitive.AddAttachment className="aui-composer-add-attachment">Attach</ComposerPrimitive.AddAttachment>
          <input 
            className="aui-composer-input" 
            value={text} 
            onChange={e => composerRuntime.setText(e.target.value)} 
            placeholder="Type a message (queued)..." 
            style={{ flex: 1 }}
          />
          <button type="submit" className="aui-composer-send">Queue</button>
          <CancelButton agentName={agentName} sessionId={sessionId} />
        </form>
      ) : (
        <>
          <div className="aui-composer-attachments">
             <ComposerPrimitive.Attachments components={{}} />
          </div>
          <ComposerPrimitive.AddAttachment className="aui-composer-add-attachment">Attach</ComposerPrimitive.AddAttachment>
          <ComposerPrimitive.Input className="aui-composer-input" placeholder="Type a message..." />
          <ComposerPrimitive.Send className="aui-composer-send">Send</ComposerPrimitive.Send>
          <CancelButton agentName={agentName} sessionId={sessionId} />
        </>
      )}
    </ComposerPrimitive.Root>
  );
};

const RunStateMonitor = ({ onRunFinish }: { onRunFinish: () => void }) => {
  const isRunning = useThread((s) => s.isRunning);
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
  if (!statusText) return null;
  return <div className="aui-status-indicator" style={{ padding: '4px 8px', fontSize: '0.85em', color: '#666', fontStyle: 'italic' }}>{statusText}</div>;
};

const SendErrorIndicator = () => {
  const { errorText } = useContext(PendingContext);
  if (!errorText) return null;
  return <div role="alert" className="aui-error" data-testid="send-error" style={{ margin: '8px' }}>{errorText}</div>;
};

const BatchInterruptUI = () => {
  const interrupts = useAgUiInterrupts();
  const submitResponses = useAgUiSubmitInterruptResponses();
  const [responses, setResponses] = useState<Record<string, 'resolved' | 'cancelled'>>({});

  useEffect(() => {
    if (!interrupts || interrupts.length === 0) {
      setResponses({});
    }
  }, [interrupts]);

  if (!interrupts || interrupts.length === 0) return null;

  const handleSubmit = () => {
     const payload = interrupts.map(i => ({
        interruptId: i.id,
        status: responses[i.id] || 'cancelled'
     }));
     submitResponses(payload);
  };

  return (
    <div className="aui-interrupts-batch" style={{ padding: '12px', borderTop: '1px solid #ccc', background: '#fefefe' }}>
      <h4 style={{ margin: '0 0 8px 0' }}>Action Required: Approve Tool Calls</h4>
      {interrupts.map((interrupt) => (
        <div key={interrupt.id} className="aui-interrupt" style={{ marginBottom: '8px', padding: '8px', border: '1px solid #eee', borderRadius: '4px' }}>
          <p style={{ margin: '0 0 4px 0', fontSize: '0.9em' }}>Tool: <strong>{interrupt.toolCallId || interrupt.reason}</strong></p>
          {interrupt.message && <pre style={{ fontSize: '0.8em', margin: '4px 0', background: '#f5f5f5', padding: '4px' }}>{interrupt.message}</pre>}
          <div className="aui-interrupt-actions" style={{ display: 'flex', gap: '12px', fontSize: '0.9em' }}>
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
        style={{ padding: '6px 12px', cursor: 'pointer' }}
      >
        Submit Decisions
      </button>
    </div>
  );
};

const MyThread = ({ agentName, sessionId, onRunFinish }: { agentName: string, sessionId: string, onRunFinish: () => void }) => {
  return (
    <ThreadPrimitive.Root className="aui-thread">
      <RunStateMonitor onRunFinish={onRunFinish} />
      <StatusIndicator />
      <ThreadPrimitive.Viewport className="aui-thread-viewport">
        <ThreadPrimitive.Messages components={{ Message: MyMessage }} />
      </ThreadPrimitive.Viewport>
      <BatchInterruptUI />
      <SendErrorIndicator />
      <MyComposer agentName={agentName} sessionId={sessionId} />
    </ThreadPrimitive.Root>
  );
};

const AgentSelector = ({
  agents,
  agentsError,
  selectedAgent,
  onSelect
}: {
  agents: Agent[];
  agentsError: string | null;
  selectedAgent: string;
  onSelect: (agent: string) => void;
}) => (
  <div className="sidebar-section">
    <h3>Agents</h3>
    {agentsError ? (
      <div role="alert" className="aui-error" data-testid="agents-error">{agentsError}</div>
    ) : (
      <select 
        value={selectedAgent} 
        onChange={e => onSelect(e.target.value)}
      >
        {agents.map(a => <option key={a.name} value={a.name}>{a.name}</option>)}
      </select>
    )}
  </div>
);

const SessionList = ({
  sessions,
  sessionsError,
  selectedSessionId,
  onSelect,
  onNewChat
}: {
  sessions: SessionRef[];
  sessionsError: string | null;
  selectedSessionId: string;
  onSelect: (id: string) => void;
  onNewChat: () => void;
}) => (
  <div className="sidebar-section">
    <div className="sessions-header">
      <h3>Sessions</h3>
      <button onClick={onNewChat}>New Chat</button>
    </div>
    {sessionsError ? (
      <div role="alert" className="aui-error" data-testid="sessions-error">{sessionsError}</div>
    ) : (
      <ul className="sessions-list">
        {sessions.map(s => (
          <li 
            key={s.session_id} 
            className={s.session_id === selectedSessionId ? 'active' : ''}
            onClick={() => onSelect(s.session_id)}
          >
            {s.session_id}
          </li>
        ))}
        {selectedSessionId && !sessions.find(s => s.session_id === selectedSessionId) && (
          <li className="active">
            {selectedSessionId} (New)
          </li>
        )}
      </ul>
    )}
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
    newChat
  } = useAgentSessions();

  return (
    <div className="app-layout">
      <div className="sidebar">
        <h2>Harnx UI</h2>
        <AgentSelector 
          agents={agents} 
          agentsError={agentsError}
          selectedAgent={selectedAgent} 
          onSelect={selectAgent} 
        />
        <SessionList 
          sessions={sessions} 
          sessionsError={sessionsError}
          selectedSessionId={selectedSessionId} 
          onSelect={selectSession} 
          onNewChat={newChat} 
        />
      </div>
      
      <div className="main-pane">
        {selectedAgent && selectedSessionId ? (
          <ChatProvider agentName={selectedAgent} sessionId={selectedSessionId}>
            <MyThread agentName={selectedAgent} sessionId={selectedSessionId} onRunFinish={refreshSessions} />
          </ChatProvider>
        ) : (
          <div className="empty-pane">Select an agent and session</div>
        )}
      </div>
    </div>
  );
}
