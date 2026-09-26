import type { SessionRef } from './types';

// Activate a click-like handler from keyboard (Enter / Space) so div-based
// "button" affordances (picker cards) are usable without mouse.
function activateOnKey(e: React.KeyboardEvent, action: () => void) {
  if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault();
    action();
  }
}

export function formatSessionContext(
  repository?: string | null,
  branch?: string | null,
): string | null {
  const repo = repository?.trim() || null;
  const br = branch?.trim() || null;
  if (repo && br) {
    return `${repo} @ ${br}`;
  }
  if (repo) {
    return repo;
  }
  if (br) {
    return `branch ${br}`;
  }
  return null;
}

export function compareSessionsUnreadFirst(a: SessionRef, b: SessionRef): number {
  const aUnread = a.unread ? 1 : 0;
  const bUnread = b.unread ? 1 : 0;
  if (aUnread !== bUnread) {
    return bUnread - aUnread;
  }
  const aTime = a.updated_at ? new Date(a.updated_at).getTime() : 0;
  const bTime = b.updated_at ? new Date(b.updated_at).getTime() : 0;
  return bTime - aTime;
}

export interface SessionCardProps {
  session: SessionRef;
  onSelect: (sessionId: string) => void;
  onToggleUnread?: (sessionId: string, currentUnread: boolean) => void;
}

export const SessionCard = ({ session, onSelect, onToggleUnread }: SessionCardProps) => {
  const isUnread = Boolean(session.unread);
  const hasTitle = Boolean(session.title && session.title.trim().length > 0);
  const contextText = formatSessionContext(session.repository, session.branch);

  return (
    <div
      className="grid-item session-card"
      onClick={() => onSelect(session.session_id)}
    >
      <div
        className="session-item session-item-main"
        role="button"
        tabIndex={0}
        onClick={(e) => {
          e.stopPropagation();
          onSelect(session.session_id);
        }}
        onKeyDown={(e) => activateOnKey(e, () => onSelect(session.session_id))}
      >
        <div className="session-item-header">
          <h3>{session.session_id}</h3>
          {isUnread && (
            <span className="session-unread-badge" data-testid="session-unread-badge">
              Unread
            </span>
          )}
        </div>
        {hasTitle && (
          <div className="session-title" data-testid="session-title">
            {session.title}
          </div>
        )}
        {contextText && (
          <div className="session-context" data-testid="session-context">
            {contextText}
          </div>
        )}
        {session.updated_at && <p>Updated: {new Date(session.updated_at).toLocaleString()}</p>}
      </div>
      {onToggleUnread && (
        <div className="session-item-actions">
          <button
            type="button"
            className="session-mark-btn"
            onClick={(e) => {
              e.stopPropagation();
              onToggleUnread(session.session_id, isUnread);
            }}
            onKeyDown={(e) => {
              e.stopPropagation();
            }}
            aria-label={isUnread ? `Mark session ${session.session_id} as read` : `Mark session ${session.session_id} as unread`}
          >
            {isUnread ? 'Mark read' : 'Mark unread'}
          </button>
        </div>
      )}
    </div>
  );
};
