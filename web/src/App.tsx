import { useEffect, useRef } from 'react';
import {
  ThreadPrimitive,
  MessagePrimitive,
  ComposerPrimitive,
  useThread
} from '@assistant-ui/react';
import { ChatProvider } from './ChatProvider';
import { cancel } from './api';
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
  return (
    <ComposerPrimitive.Root className="aui-composer">
      <ComposerPrimitive.Input className="aui-composer-input" placeholder="Type a message..." />
      <ComposerPrimitive.Send className="aui-composer-send">Send</ComposerPrimitive.Send>
      <CancelButton agentName={agentName} sessionId={sessionId} />
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

const MyThread = ({ agentName, sessionId, onRunFinish }: { agentName: string, sessionId: string, onRunFinish: () => void }) => {
  return (
    <ThreadPrimitive.Root className="aui-thread">
      <RunStateMonitor onRunFinish={onRunFinish} />
      <ThreadPrimitive.Viewport className="aui-thread-viewport">
        <ThreadPrimitive.Messages components={{ Message: MyMessage }} />
      </ThreadPrimitive.Viewport>
      <MyComposer agentName={agentName} sessionId={sessionId} />
    </ThreadPrimitive.Root>
  );
};

const AgentSelector = ({
  agents,
  selectedAgent,
  onSelect
}: {
  agents: Agent[];
  selectedAgent: string;
  onSelect: (agent: string) => void;
}) => (
  <div className="sidebar-section">
    <h3>Agents</h3>
    <select 
      value={selectedAgent} 
      onChange={e => onSelect(e.target.value)}
    >
      {agents.map(a => <option key={a.name} value={a.name}>{a.name}</option>)}
    </select>
  </div>
);

const SessionList = ({
  sessions,
  selectedSessionId,
  onSelect,
  onNewChat
}: {
  sessions: SessionRef[];
  selectedSessionId: string;
  onSelect: (id: string) => void;
  onNewChat: () => void;
}) => (
  <div className="sidebar-section">
    <div className="sessions-header">
      <h3>Sessions</h3>
      <button onClick={onNewChat}>New Chat</button>
    </div>
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
  </div>
);

export default function App() {
  const {
    agents,
    sessions,
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
          selectedAgent={selectedAgent} 
          onSelect={selectAgent} 
        />
        <SessionList 
          sessions={sessions} 
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
