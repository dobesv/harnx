import type { KeyboardEvent } from 'react';
import type { SubAgentNote } from './subAgentNotes';

export interface SubAgentSessionNotesProps {
  notes: SubAgentNote[];
  onOpen: (agent: string, sessionId: string) => void;
}

const STATUS_LABEL = {
  running: 'Running',
  done: 'Done',
  failed: 'Failed',
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

export function SubAgentSessionNotes({ notes, onOpen }: SubAgentSessionNotesProps) {
  if (notes.length === 0) return null;

  return (
    <div className="aui-sub-agent-notes" aria-label="Sub-agent sessions">
      {notes.map((note) => {
        const statusLabel = STATUS_LABEL[note.status];
        const open = () => onOpen(note.agent, note.sessionId);
        return (
          <button
            type="button"
            className="aui-sub-agent-note"
            data-status={note.status}
            key={note.id}
            aria-label={`Open ${note.agent} sub-agent session ${note.sessionId} (${statusLabel.toLowerCase()})`}
            onClick={open}
            onKeyDown={(event) => activateOnKey(event, open)}
          >
            <span className="aui-sub-agent-identity">
              <span className="aui-sub-agent-name">{note.agent}</span>
              <span className="aui-sub-agent-session">{note.sessionId}</span>
            </span>
            <span className={`aui-sub-agent-status aui-sub-agent-status-${note.status}`}>
              <span className="aui-sub-agent-status-icon" aria-hidden="true" />
              {statusLabel}
            </span>
          </button>
        );
      })}
    </div>
  );
}
