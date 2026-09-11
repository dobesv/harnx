import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';
import { AgentPicker } from '../App';
import * as connectionStatusModule from '../useConnectionStatus';

describe('AgentPicker reconnecting states', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders connecting message when not loaded and nextRetryAt is null', () => {
    vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
      status: 'reconnecting',
      attempt: 1,
      nextRetryAt: null,
      retryingReads: 1,
    });
    vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(null);

    render(<AgentPicker agents={[]} agentsError={null} hasLoadedAgents={false} onSelect={vi.fn()} />);
    const connecting = screen.getByTestId('agents-connecting');
    expect(connecting).toBeInTheDocument();
    expect(connecting).toHaveAttribute('role', 'status');
    expect(connecting).toHaveTextContent('Connecting to server…');
  });

  it('renders reconnecting countdown message when not loaded and countdown is active', () => {
    vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
      status: 'reconnecting',
      attempt: 2,
      nextRetryAt: Date.now() + 5000,
      retryingReads: 1,
    });
    vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(5);

    render(<AgentPicker agents={[]} agentsError={null} hasLoadedAgents={false} onSelect={vi.fn()} />);
    const connecting = screen.getByTestId('agents-connecting');
    expect(connecting).toBeInTheDocument();
    expect(connecting).toHaveTextContent('Reconnecting… retrying in 5s');
  });

  it('renders valid empty state when hasLoadedAgents is true and agents is empty', () => {
    render(<AgentPicker agents={[]} agentsError={null} hasLoadedAgents={true} onSelect={vi.fn()} />);
    expect(screen.queryByTestId('agents-connecting')).toBeNull();
    expect(screen.getByText('No agents found.')).toBeInTheDocument();
  });

  it('renders agentsError alert when agentsError is provided even if not loaded', () => {
    render(<AgentPicker agents={[]} agentsError="Server exploded" hasLoadedAgents={false} onSelect={vi.fn()} />);
    expect(screen.queryByTestId('agents-connecting')).toBeNull();
    const err = screen.getByTestId('agents-error');
    expect(err).toBeInTheDocument();
    expect(err).toHaveAttribute('role', 'alert');
    expect(err).toHaveTextContent('Server exploded');
  });

  it('renders agent list when loaded and agents exist', () => {
    const agents = [{ name: 'agent-1', description: 'Test agent' } as any];
    render(<AgentPicker agents={agents} agentsError={null} hasLoadedAgents={true} onSelect={vi.fn()} />);
    expect(screen.queryByTestId('agents-connecting')).toBeNull();
    expect(screen.getByText('agent-1')).toBeInTheDocument();
    expect(screen.getByText('Test agent')).toBeInTheDocument();
  });
});
