import React from 'react';
import { LinkButton } from './LinkButton';
import { OpenInNewIcon } from './icons';

export interface HandoffRowProps {
  agent: string;
  sessionId: string;
  onNavigate: (agent: string, sessionId: string) => void;
}

export const HandoffRow: React.FC<HandoffRowProps> = ({ agent, sessionId, onNavigate }) => {
  const href = `/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(sessionId)}`;

  return (
    <div className="aui-handoff-row" data-testid="handoff-row">
      <div className="aui-handoff-identity">
        <span className="aui-handoff-agent">{agent}</span>
        <span className="aui-handoff-session">{sessionId}</span>
      </div>
      <LinkButton
        href={href}
        onNavigate={() => onNavigate(agent, sessionId)}
        className="aui-sub-agent-open aui-handoff-open"
        aria-label={`Open ${agent} session ${sessionId}`}
        title={`Open ${agent} session ${sessionId}`}
      >
        <OpenInNewIcon />
      </LinkButton>
    </div>
  );
};
