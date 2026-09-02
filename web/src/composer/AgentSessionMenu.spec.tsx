import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { AgentDropdown, SessionDropdown, AgentSessionMenu } from './AgentSessionMenu';
import { vi, describe, it, expect, beforeAll, beforeEach } from 'vitest';

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
    
    await user.click(trigger);

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
});
