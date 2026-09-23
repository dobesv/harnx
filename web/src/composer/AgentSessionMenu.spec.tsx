import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { AgentDropdown, SessionDropdown, AgentSessionMenu } from './AgentSessionMenu';
import { vi, describe, it, expect, beforeAll, beforeEach } from 'vitest';
import { CompactionContext } from '../CompactionContext';
import { PendingContext } from '../PendingContext';

// Polyfills for Radix in jsdom
beforeAll(() => {
  if (!Element.prototype.hasPointerCapture) {
    Element.prototype.hasPointerCapture = () => false;
  }
  if (!Element.prototype.setPointerCapture) {
    Element.prototype.setPointerCapture = () => {};
  }
  if (!Element.prototype.releasePointerCapture) {
    Element.prototype.releasePointerCapture = () => {};
  }
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = () => {};
  }
  globalThis.ResizeObserver = class ResizeObserver {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
});

// Mock compactSession while preserving other exports (e.g. formatUnchangedReason)
vi.mock('../compactionApi', async importOriginal => {
  const actual = await importOriginal<typeof import('../compactionApi')>();
  return {
    ...actual,
    compactSession: vi.fn(),
  };
});

import { compactSession } from '../compactionApi';
const mockCompactSession = vi.mocked(compactSession);

const defaultPendingContext = {
  statusText: null as string | null,
  setStatusText: vi.fn(),
  errorText: null as string | null,
  setErrorText: vi.fn(),
  statusMessage: null as string | null,
  setStatusMessage: vi.fn(),
  hydratedApprovals: [] as Array<{ toolCallId: string; summary: string }>,
  addHydratedApproval: vi.fn(),
  clearHydratedApprovals: vi.fn(),
  removeHydratedApproval: vi.fn(),
};

const defaultCompactionContext = {
  phase: 'idle' as const,
  compactionId: undefined,
};

function renderWithProviders(
  ui: React.ReactElement,
  compaction: { phase: 'idle' | 'compacting' | 'failed'; compactionId?: string } = defaultCompactionContext
) {
  return render(
    <PendingContext.Provider value={defaultPendingContext}>
      <CompactionContext.Provider value={compaction}>{ui}</CompactionContext.Provider>
    </PendingContext.Provider>
  );
}

describe('AgentSessionMenu', () => {
  const defaultProps = {
    agentName: 'coding/coder',
    sessionId: 'test-session-id',
    switchAgentHref: '/',
    switchSessionHref: `/agents/${encodeURIComponent('coding/coder')}`,
    onSwitchAgent: vi.fn(),
    onSwitchSession: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('AgentDropdown works and hrefs are correct', async () => {
    const user = userEvent.setup();
    render(<AgentDropdown {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Agent: coding/coder' });
    
    // Open menu with Enter
    trigger.focus();
    await user.keyboard('{Enter}');

    // Label should be visible in the menu
    const menu = screen.getByRole('menu');
    expect(menu).toHaveTextContent('coding/coder');

    const switchItem = screen.getByRole('menuitem', { name: /switch agent/i });
    expect(switchItem).toHaveAttribute('href', '/');

    // Escape closes and restores focus
    await user.keyboard('{Escape}');
    await waitFor(() => {
      expect(screen.queryByRole('menuitem', { name: /switch agent/i })).not.toBeInTheDocument();
    });
    expect(document.activeElement).toBe(trigger);

    // Reopen to test click
    trigger.focus();
    await user.keyboard('{Enter}');
    const switchItemAfter = screen.getByRole('menuitem', { name: /switch agent/i });

    // Click behavior - Left click default prevented check
    const clickEvent = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 });
    const notPrevented = switchItemAfter.dispatchEvent(clickEvent);
    expect(notPrevented).toBe(false); // default is prevented

    // Fire actual click to test handler (since Radix triggers it)
    await user.click(switchItemAfter);
    expect(defaultProps.onSwitchAgent).toHaveBeenCalledTimes(1);
  });

  it('SessionDropdown works and hrefs are correct', async () => {
    const user = userEvent.setup();
    render(<SessionDropdown {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    
    // Open menu with ArrowDown
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    // Label should be visible
    const menu = screen.getByRole('menu');
    expect(menu).toHaveTextContent('test-session-id');

    const switchItem = screen.getByRole('menuitem', { name: /switch session/i });
    expect(switchItem).toHaveAttribute('href', '/agents/coding%2Fcoder');

    // Click behavior - Left click default prevented check
    const clickEvent = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 });
    const notPrevented = switchItem.dispatchEvent(clickEvent);
    expect(notPrevented).toBe(false); // default is prevented

    // Fire actual click to test handler (since Radix triggers it)
    await user.click(switchItem);
    expect(defaultProps.onSwitchSession).toHaveBeenCalledTimes(1);
  });

  it('AgentSessionMenu combined works with modifiers', async () => {
    const user = userEvent.setup();
    render(<AgentSessionMenu {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Agent and session options' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const menu = screen.getByRole('menu');
    expect(menu).toHaveTextContent('coding/coder');
    expect(menu).toHaveTextContent('test-session-id');

    const switchAgent = screen.getByRole('menuitem', { name: /switch agent/i });
    const switchSession = screen.getByRole('menuitem', { name: /switch session/i });

    expect(switchAgent).toHaveAttribute('href', '/');
    expect(switchSession).toHaveAttribute('href', '/agents/coding%2Fcoder');

    // Ctrl click doesn't trigger handler
    await user.keyboard('{Control>}');
    await user.click(switchAgent);
    await user.keyboard('{/Control}');
    
    expect(defaultProps.onSwitchAgent).not.toHaveBeenCalled();
  });

  it('renders unread indicator dot in SessionDropdown and AgentSessionMenu when unread is true', () => {
    const { unmount } = render(<SessionDropdown {...defaultProps} unread={true} />);
    const dot = screen.getByTestId('current-session-unread-dot');
    expect(dot).toBeInTheDocument();
    expect(dot).toHaveAttribute('aria-hidden', 'true');
    expect(screen.getByRole('button', { name: 'Session: test-session-id (unread)' })).toBeInTheDocument();
    unmount();

    render(<SessionDropdown {...defaultProps} unread={false} />);
    expect(screen.queryByTestId('current-session-unread-dot')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Session: test-session-id' })).toBeInTheDocument();

    render(<AgentSessionMenu {...defaultProps} unread={true} />);
    const mobileDot = screen.getByTestId('current-session-unread-dot-mobile');
    expect(mobileDot).toBeInTheDocument();
    expect(mobileDot).toHaveAttribute('aria-hidden', 'true');
    expect(screen.getByRole('button', { name: 'Agent and session options (unread)' })).toBeInTheDocument();
  });
});

describe('CompactSessionItem', () => {
  const defaultProps = {
    agentName: 'coding/coder',
    sessionId: 'test-session-id',
    switchAgentHref: '/',
    switchSessionHref: `/agents/${encodeURIComponent('coding/coder')}`,
    onSwitchAgent: vi.fn(),
    onSwitchSession: vi.fn(),
  };

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders "Compact session" menuitem in SessionDropdown', async () => {
    const user = userEvent.setup();
    renderWithProviders(<SessionDropdown {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const compactItem = screen.getByRole('menuitem', { name: /compact session/i });
    expect(compactItem).toBeInTheDocument();
  });

  it('calls compactSession when clicking "Compact session"', async () => {
    mockCompactSession.mockResolvedValueOnce({ status: 'submitted', compaction_id: 'c1' });
    const user = userEvent.setup();
    renderWithProviders(<SessionDropdown {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const compactItem = screen.getByRole('menuitem', { name: /compact session/i });
    await user.click(compactItem);

    expect(mockCompactSession).toHaveBeenCalledWith('coding/coder', 'test-session-id');
  });

  it('is disabled when compaction is in progress', async () => {
    const user = userEvent.setup();
    renderWithProviders(<SessionDropdown {...defaultProps} />, { phase: 'compacting' });

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const compactItem = screen.getByRole('menuitem', { name: /compact session/i });
    expect(compactItem).toHaveAttribute('aria-disabled', 'true');
  });

  it('shows spinner when compaction is in progress', async () => {
    const user = userEvent.setup();
    renderWithProviders(<SessionDropdown {...defaultProps} />, { phase: 'compacting' });

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const spinner = screen.getByRole('menuitem', { name: /compact session/i }).querySelector('.aui-spinner');
    expect(spinner).toBeInTheDocument();
  });

  it('shows "Compaction already in progress" message when already_in_flight', async () => {
    mockCompactSession.mockResolvedValueOnce({ status: 'already_in_flight', compaction_id: 'c1' });
    const user = userEvent.setup();
    renderWithProviders(<SessionDropdown {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const compactItem = screen.getByRole('menuitem', { name: /compact session/i });
    await user.click(compactItem);

    await waitFor(() => {
      expect(defaultPendingContext.setStatusMessage).toHaveBeenCalledWith('Compaction already in progress');
    });
  });

  it('shows formatted message when nothing_to_do with unchanged outcome', async () => {
    mockCompactSession.mockResolvedValueOnce({
      status: 'nothing_to_do',
      outcome: { status: 'unchanged', detail: 'no_user_messages' },
    });
    const user = userEvent.setup();
    renderWithProviders(<SessionDropdown {...defaultProps} />);

    const trigger = screen.getByRole('button', { name: 'Session: test-session-id' });
    trigger.focus();
    await user.keyboard('{ArrowDown}');

    const compactItem = screen.getByRole('menuitem', { name: /compact session/i });
    await user.click(compactItem);

    await waitFor(() => {
      expect(defaultPendingContext.setStatusMessage).toHaveBeenCalledWith('No user messages to compact');
    });
  });
});
