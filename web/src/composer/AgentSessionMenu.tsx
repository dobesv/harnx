import { useContext } from 'react';
import * as DropdownMenu from '@radix-ui/react-dropdown-menu';
import { ChevronDownIcon, MenuIcon } from '../icons';
import { LinkButton } from '../LinkButton';
import { CompactionContext } from '../CompactionContext';
import { compactSession, formatUnchangedReason } from '../compactionApi';
import { PendingContext } from '../PendingContext';

export interface AgentSessionMenuProps {
  agentName: string;
  sessionId: string;
  unread?: boolean;
  switchAgentHref: string;
  switchSessionHref: string;
  onSwitchAgent: () => void;
  onSwitchSession: () => void;
}

export function AgentDropdown(props: AgentSessionMenuProps) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="aui-composer-menu-trigger" aria-label={`Agent: ${props.agentName}`}>
          <span>{props.agentName}</span>
          <ChevronDownIcon />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="aui-composer-menu-content" sideOffset={6} collisionPadding={8}>
          <DropdownMenu.Label className="aui-composer-menu-label">{props.agentName}</DropdownMenu.Label>
          <DropdownMenu.Item asChild className="aui-composer-menu-item">
            <LinkButton href={props.switchAgentHref} onNavigate={props.onSwitchAgent}>
              Switch agent…
            </LinkButton>
          </DropdownMenu.Item>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

function CompactSessionItem({ agentName, sessionId }: { agentName: string; sessionId: string }) {
  const compaction = useContext(CompactionContext);
  const { setStatusMessage, setErrorText } = useContext(PendingContext);

  const handleCompact = async () => {
    try {
      const result = await compactSession(agentName, sessionId);
      if (result.status === 'already_in_flight') {
        setStatusMessage('Compaction already in progress');
      } else if (result.status === 'nothing_to_do') {
        setStatusMessage(formatUnchangedReason(result.outcome.detail));
      }
      // submitted: spinner will show via CompactionContext
    } catch (err) {
      setErrorText(err instanceof Error ? err.message : 'Failed to compact session');
    }
  };

  const isCompacting = compaction.phase === 'compacting';

  return (
    <DropdownMenu.Item
      className="aui-composer-menu-item"
      onSelect={handleCompact}
      disabled={isCompacting}
    >
      {isCompacting ? (
        <span className="aui-spinner"><span></span></span>
      ) : null}
      Compact session
    </DropdownMenu.Item>
  );
}

export function SessionDropdown(props: AgentSessionMenuProps) {
  const sessionLabel = `Session: ${props.sessionId}${props.unread ? ' (unread)' : ''}`;
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="aui-composer-menu-trigger" aria-label={sessionLabel}>
          {props.unread && (
            <span
              className="session-unread-dot"
              aria-hidden="true"
              data-testid="current-session-unread-dot"
            />
          )}
          <span>{props.sessionId}</span>
          <ChevronDownIcon />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="aui-composer-menu-content" sideOffset={6} collisionPadding={8}>
          <DropdownMenu.Label className="aui-composer-menu-label">{props.sessionId}</DropdownMenu.Label>
          <DropdownMenu.Item asChild className="aui-composer-menu-item">
            <LinkButton href={props.switchSessionHref} onNavigate={props.onSwitchSession}>
              Switch session…
            </LinkButton>
          </DropdownMenu.Item>
          <CompactSessionItem agentName={props.agentName} sessionId={props.sessionId} />
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

export function AgentSessionMenu(props: AgentSessionMenuProps) {
  const optionsLabel = `Agent and session options${props.unread ? ' (unread)' : ''}`;
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="aui-composer-menu-trigger aui-composer-menu-trigger-icon" aria-label={optionsLabel}>
          {props.unread && (
            <span
              className="session-unread-dot"
              aria-hidden="true"
              data-testid="current-session-unread-dot-mobile"
              style={{ position: 'absolute', top: '6px', right: '6px' }}
            />
          )}
          <MenuIcon />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="aui-composer-menu-content" sideOffset={6} collisionPadding={8}>
          <DropdownMenu.Group>
            <DropdownMenu.Label className="aui-composer-menu-label">{props.agentName}</DropdownMenu.Label>
            <DropdownMenu.Item asChild className="aui-composer-menu-item">
              <LinkButton href={props.switchAgentHref} onNavigate={props.onSwitchAgent}>
                Switch agent…
              </LinkButton>
            </DropdownMenu.Item>
          </DropdownMenu.Group>
          <DropdownMenu.Separator className="aui-composer-menu-separator" />
          <DropdownMenu.Group>
            <DropdownMenu.Label className="aui-composer-menu-label">{props.sessionId}</DropdownMenu.Label>
            <DropdownMenu.Item asChild className="aui-composer-menu-item">
              <LinkButton href={props.switchSessionHref} onNavigate={props.onSwitchSession}>
                Switch session…
              </LinkButton>
            </DropdownMenu.Item>
            <CompactSessionItem agentName={props.agentName} sessionId={props.sessionId} />
          </DropdownMenu.Group>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
