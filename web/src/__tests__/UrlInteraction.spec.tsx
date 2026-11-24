import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { BatchInterruptUI } from '../App';
import { submitInteractionResponse } from '../api';
import type { PendingInteraction } from '../harnxCustomEvents';
import { PendingContext } from '../PendingContext';
import * as agUi from '@assistant-ui/react-ag-ui';

vi.mock('@assistant-ui/react-ag-ui', async (importOriginal) => {
  const actual = await importOriginal<typeof agUi>();
  return { ...actual, useAgUiInterrupts: vi.fn(), useAgUiSubmitInterruptResponses: vi.fn() };
});

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return {
    ...actual,
    submitInteractionResponse: vi.fn(),
  };
});

const defaultPendingContext = {
  statusText: null,
  setStatusText: vi.fn(),
  errorText: null,
  setErrorText: vi.fn(),
  statusMessage: null,
  setStatusMessage: vi.fn(),
  hydratedApprovals: [],
  addHydratedApproval: vi.fn(),
  clearHydratedApprovals: vi.fn(),
  removeHydratedApproval: vi.fn(),
  pendingInteraction: null,
  setPendingInteraction: vi.fn(),
  clearPendingInteraction: vi.fn(),
};

const sampleUrlInteraction: PendingInteraction = {
  interactionId: 'int-url-1',
  requestSeq: 3,
  toolCallId: 'tool-connect',
  kind: 'url',
  message: 'Connect Jira',
  pendingCount: 0,
  payload: { url: 'https://example.com/connect/abc', message: 'Connect Jira' },
  url: 'https://example.com/connect/abc',
};

describe('URL interaction web renderer (decision 6)', () => {
  beforeEach(() => {
    vi.mocked(submitInteractionResponse).mockReset();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([]);
  });

  it('renders a new-tab link with noopener noreferrer plus Continue/Cancel', () => {
    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: sampleUrlInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    const link = screen.getByTestId('url-interaction-link');
    expect(link).toBeInTheDocument();
    expect(link).toHaveAttribute('href', 'https://example.com/connect/abc');
    expect(link).toHaveAttribute('target', '_blank');
    expect(link).toHaveAttribute('rel', 'noopener noreferrer');
    expect(screen.getByText('Connect Jira')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Continue' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeInTheDocument();
    // A url interaction never offers Approve/Deny or a note.
    expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Deny' })).not.toBeInTheDocument();
    expect(screen.queryByLabelText('Optional Note:')).not.toBeInTheDocument();
  });

  it('submits continue with empty data and nothing beyond the action', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleUrlInteraction,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByTestId('url-interaction-continue'));

    await waitFor(() =>
      expect(submitInteractionResponse).toHaveBeenCalledWith('agent-x', 'session-y', {
        interactionId: 'int-url-1',
        requestSeq: 3,
        response: {
          action: 'continue',
          data: {},
          note: undefined,
        },
      })
    );
  });

  it('submits cancel with empty data', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleUrlInteraction,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByTestId('url-interaction-cancel'));

    await waitFor(() =>
      expect(submitInteractionResponse).toHaveBeenCalledWith('agent-x', 'session-y', {
        interactionId: 'int-url-1',
        requestSeq: 3,
        response: {
          action: 'cancel',
          data: {},
          note: undefined,
        },
      })
    );
  });
});
