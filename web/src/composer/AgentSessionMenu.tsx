import * as DropdownMenu from '@radix-ui/react-dropdown-menu';
import { ChevronDownIcon, MenuIcon } from '../icons';
import { LinkButton } from '../LinkButton';

export interface AgentSessionMenuProps {
  agentName: string;
  sessionId: string;
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

export function SessionDropdown(props: AgentSessionMenuProps) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="aui-composer-menu-trigger" aria-label={`Session: ${props.sessionId}`}>
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
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

export function AgentSessionMenu(props: AgentSessionMenuProps) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="aui-composer-menu-trigger aui-composer-menu-trigger-icon" aria-label="Agent and session options">
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
          </DropdownMenu.Group>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
