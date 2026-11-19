class MockEventSource {
  close() {}
  addEventListener() {}
}
(globalThis as any).EventSource = MockEventSource;

import { useContext, StrictMode } from 'react';
import { act, render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import {
  BatchInterruptUI,
  InteractionUI,
  CancelButton,
  MyAttachment,
  MyComposer,
  MyMessage,
  SendErrorIndicator,
  StatusBar,
  StatusIndicator,
} from '../App';
import { MarkdownLink } from '../markdownLink';
import { sendPrompt, uploadAttachment, submitHitlDecision, submitInteractionResponse, InteractionResponseError, markRead } from '../api';
import type { PendingInteraction } from '../harnxCustomEvents';
import { PendingContext } from '../PendingContext';
import { UsageContext } from '../UsageContext';
import { MessageAttachmentsContext } from '../MessageAttachmentsContext';
import { CompactionMetadataContext } from '../CompactionMetadataContext';
import * as agUi from '@assistant-ui/react-ag-ui';
import * as aui from '@assistant-ui/react';
import { vi } from 'vitest';
import { ChatProvider } from '../ChatProvider';
import { CancellationContext, type CancellationControl } from '../CancellationContext';
import { useCancellation } from '../useCancellation';

const defaultPendingContext = {
  statusText: null,
  setStatusText: vi.fn(),
  errorText: null,
  setErrorText: vi.fn(),
  statusMessage: null,
  setStatusMessage: vi.fn(),
  hydratedApprovals: [],
  addHydratedApproval: vi.fn(),
  clearHydratedApprovals: vi.fn(), removeHydratedApproval: vi.fn(),
  pendingInteraction: null,
  setPendingInteraction: vi.fn(),
  clearPendingInteraction: vi.fn(),
};

vi.mock('@assistant-ui/react-ag-ui', async (importOriginal) => {
  const actual = await importOriginal<typeof agUi>();
  return { ...actual, useAgUiInterrupts: vi.fn(), useAgUiSubmitInterruptResponses: vi.fn() };
});

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return {
    ...actual,
    sendPrompt: vi.fn(),
    uploadAttachment: vi.fn(),
    submitHitlDecision: vi.fn(),
    submitInteractionResponse: vi.fn(),
    markRead: vi.fn().mockResolvedValue({ status: 'ok' }),
    markUnread: vi.fn().mockResolvedValue({ status: 'ok' }),
  };
});
vi.mock('../useCancellation', () => ({ useCancellation: vi.fn() }));
vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return { 
    ...actual, 
    useAuiState: vi.fn(), 
    useAui: vi.fn(),
    AttachmentPrimitive: {
      Root: ({ className, children }: any) => <div className={className}>{children}</div>,
      unstable_Thumb: ({ className }: any) => <div className={className} />,
      Name: () => <span>diagram.png</span>,
      Remove: ({ className, children, ...props }: any) => (
        <button type="button" className={className} {...props}>
          {children}
        </button>
      ),
    },
    MessagePrimitive: {
      Root: ({ className, children, ...props }: any) => <div className={className} {...props}>{children}</div>,
      Content: () => null,
    },
  };
});

const cancellationControl = (
  phase: CancellationControl['phase'],
  stop = vi.fn(async () => {}),
): CancellationControl => ({ phase, stop });

describe('cancellation UI', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({ thread: { isRunning: false, messages: [] } }),
    );
  });

  it('shows Stop only for an active run and invokes cancellation', () => {
    const stop = vi.fn(async () => {});
    const { rerender } = render(
      <CancellationContext.Provider value={cancellationControl('idle', stop)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    expect(screen.queryByRole('button', { name: 'Stop' })).not.toBeInTheDocument();

    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({ thread: { isRunning: true, messages: [] } }),
    );
    rerender(
      <CancellationContext.Provider value={cancellationControl('idle', stop)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    const stopBtn = screen.getByRole('button', { name: 'Stop' });
    expect(stopBtn).toHaveClass('aui-composer-icon-btn');
    expect(stopBtn).toHaveClass('aui-cancel-button');
    fireEvent.click(stopBtn);
    expect(stop).toHaveBeenCalledOnce();
  });

  it('disables the button only while the interrupt is being appended', () => {
    render(
      <CancellationContext.Provider value={cancellationControl('requesting')}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    const requestingBtn = screen.getByRole('button', { name: 'Interrupting…' });
    expect(requestingBtn).toBeDisabled();
    expect(requestingBtn).toHaveClass('aui-composer-icon-btn');
  });

  it('offers retry after the interrupt fails to land', () => {
    const stop = vi.fn(async () => {});
    render(
      <CancellationContext.Provider value={cancellationControl('failed', stop)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    const failed = screen.getByRole('button', { name: 'Interrupt failed — Retry' });
    expect(failed).toBeEnabled();
    fireEvent.click(failed);
    expect(stop).toHaveBeenCalledOnce();
  });

  it('spins while the interrupt is in flight and stops once it has failed', () => {
    const { rerender, container } = render(
      <CancellationContext.Provider value={cancellationControl('requesting')}>
        <StatusIndicator isRunning statusText={null} />
      </CancellationContext.Provider>,
    );
    expect(screen.getByText('Interrupting…')).toBeInTheDocument();
    expect(container.querySelector('.aui-spinner')).toBeInTheDocument();

    rerender(
      <CancellationContext.Provider value={cancellationControl('failed')}>
        <StatusIndicator isRunning statusText={null} />
      </CancellationContext.Provider>,
    );
    expect(container.querySelector('.aui-spinner')).not.toBeInTheDocument();
  });
});



describe('BatchInterruptUI', () => {
  afterEach(() => {
    vi.clearAllMocks();
  });

  it('submits exact payload for approve via submitHitlDecision and clears live gate', async () => {
    const setErrorText = vi.fn();
    const removeHydratedApproval = vi.fn();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-1', toolCallId: 'tool-1', reason: '', message: 'live interrupt' } as any]);
    vi.mocked(submitHitlDecision).mockResolvedValue({ applied: true });

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText, removeHydratedApproval }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByRole('button', { name: 'Approve' })).toHaveAttribute('type', 'button');
    expect(screen.getByRole('button', { name: 'Deny' })).toHaveAttribute('type', 'button');

    fireEvent.click(screen.getByText('Approve'));

    await waitFor(() => {
      expect(submitHitlDecision).toHaveBeenCalledWith('test-agent', 'test-session', { toolCallId: 'tool-1', approved: true, note: undefined });
      expect(removeHydratedApproval).toHaveBeenCalledWith('tool-1');
    });

    expect(defaultPendingContext.setStatusText).not.toHaveBeenCalled();
    // After approval, the live interrupt gate clears and returns null
    expect(screen.queryByTestId('hydrated-pending-approval')).not.toBeInTheDocument();
  });

  it('handles applied: false by setting status notice and clearing live gate', async () => {
    const setStatusText = vi.fn();
    const setErrorText = vi.fn();
    const removeHydratedApproval = vi.fn();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-1', toolCallId: 'tool-1', reason: '', message: 'live interrupt' } as any]);
    vi.mocked(submitHitlDecision).mockResolvedValue({ applied: false });

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, setStatusText, setErrorText, removeHydratedApproval }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByText('Approve'));

    await waitFor(() => {
      expect(submitHitlDecision).toHaveBeenCalledWith('test-agent', 'test-session', { toolCallId: 'tool-1', approved: true, note: undefined });
      expect(setStatusText).toHaveBeenCalledWith('This approval was already resolved elsewhere.');
      expect(removeHydratedApproval).toHaveBeenCalledWith('tool-1');
    });

    // Gate clears locally even when applied is false
    expect(screen.queryByTestId('hydrated-pending-approval')).not.toBeInTheDocument();
  });

  it('submits deny payload with note and associates label with input', async () => {
    const setErrorText = vi.fn();
    const removeHydratedApproval = vi.fn();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-1', toolCallId: 'tool-1', reason: '', message: 'live interrupt' } as any]);
    vi.mocked(submitHitlDecision).mockResolvedValue({ applied: true });

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText, removeHydratedApproval }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // Verify label association (a11y)
    const noteInput = screen.getByLabelText('Optional Note:');
    expect(noteInput).toBeInTheDocument();
    fireEvent.change(noteInput, { target: { value: 'unsafe tool call' } });

    fireEvent.click(screen.getByText('Deny'));

    await waitFor(() => {
      expect(submitHitlDecision).toHaveBeenCalledWith('test-agent', 'test-session', {
        toolCallId: 'tool-1',
        approved: false,
        note: 'unsafe tool call',
      });
      expect(removeHydratedApproval).toHaveBeenCalledWith('tool-1');
    });

    expect(screen.queryByTestId('hydrated-pending-approval')).not.toBeInTheDocument();
  });

  const renderBatchInterruptWithDecisionError = (error: unknown) => {
    const setErrorText = vi.fn();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-2', toolCallId: 'tool-2', reason: '' } as any]);
    vi.mocked(submitHitlDecision).mockRejectedValue(error);

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByText('Approve'));
    return setErrorText;
  };

  it('surfaces an error on rejection', async () => {
    const setErrorText = renderBatchInterruptWithDecisionError(new Error('Network error'));
    await waitFor(() => {
      expect(setErrorText).toHaveBeenCalledWith('Network error');
    });
  });

  it('does not surface an error when submitHitlDecision is aborted (#1838)', async () => {
    const setErrorText = renderBatchInterruptWithDecisionError(
      new DOMException('signal is aborted without reason', 'AbortError')
    );

    await waitFor(() => {
      expect(submitHitlDecision).toHaveBeenCalled();
    });
    expect(setErrorText).not.toHaveBeenCalledWith(expect.any(String));
  });

  it('renders hydrated pending approval from hitl_pending_approval CUSTOM event', () => {
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([]);

    const hydratedApprovals = [
      { toolCallId: 'tool-1', summary: 'Approve tool call: write_file' },
    ];

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, hydratedApprovals }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByTestId('hydrated-pending-approval')).toBeInTheDocument();
    expect(screen.getByText(/Approve tool call: write_file/)).toBeInTheDocument();
  });

  it('dedupes hydrated approval when live interrupt has matching toolCallId, showing only one', () => {
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([
      { id: 'int-1', toolCallId: 'tool-1', reason: 'tool_call', message: 'Approve this' } as any,
    ]);

    const hydratedApprovals = [
      { toolCallId: 'tool-1', summary: 'Approve tool call' },
    ];

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, hydratedApprovals }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // It uses the hydrated one's summary if both exist (because it adds hydrated first)
    expect(screen.getByText(/Approve tool call/)).toBeInTheDocument();
    // Only ONE approve button should exist
    expect(screen.getAllByText('Approve').length).toBe(1);
  });

  it('advances to next pending item when first item is decided', async () => {
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([
      { id: 'int-1', toolCallId: 'tool-1', reason: '', message: 'Tool 1 interrupt' } as any,
      { id: 'int-2', toolCallId: 'tool-2', reason: '', message: 'Tool 2 interrupt' } as any,
    ]);
    vi.mocked(submitHitlDecision).mockResolvedValue({ applied: true });

    render(
      <PendingContext.Provider value={defaultPendingContext}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByText('Tool 1 interrupt')).toBeInTheDocument();
    expect(screen.getByText('1 more tool call awaiting approval')).toBeInTheDocument();

    fireEvent.click(screen.getByText('Approve'));

    await waitFor(() => {
      expect(submitHitlDecision).toHaveBeenCalledWith('test-agent', 'test-session', { toolCallId: 'tool-1', approved: true, note: undefined });
    });

    // After tool-1 is resolved, tool-2 is displayed
    await waitFor(() => {
      expect(screen.getByText('Tool 2 interrupt')).toBeInTheDocument();
    });
    expect(screen.queryByText(/more tool call/)).not.toBeInTheDocument();
  });
});

describe('InteractionUI & BatchInterruptUI (interactions)', () => {
  beforeEach(() => {
    vi.mocked(submitInteractionResponse).mockReset();
    vi.mocked(submitHitlDecision).mockReset();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([]);
  });

  const sampleInteraction: PendingInteraction = {
    interactionId: 'int-123',
    requestSeq: 5,
    toolCallId: 'tool-xyz',
    kind: 'confirm',
    message: 'Allow executing rm -rf?',
    pendingCount: 0,
  };

  it('renders approve/deny buttons, message, and optional note for confirm kind', () => {
    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: sampleInteraction }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByTestId('pending-interaction-card')).toBeInTheDocument();
    expect(screen.getByText('Allow executing rm -rf?')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Approve' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Deny' })).toBeInTheDocument();
    expect(screen.getByPlaceholderText('Reason for approval or denial...')).toBeInTheDocument();
    expect(screen.queryByText(/not supported by this client/i)).not.toBeInTheDocument();
  });

  it('submits approve action without note when note is empty', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: sampleInteraction, setPendingInteraction }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith('test-agent', 'test-session', {
        interactionId: 'int-123',
        requestSeq: 5,
        response: {
          action: 'approve',
          data: {},
          note: undefined,
        },
      });
    });

    const updater = setPendingInteraction.mock.calls[0][0];
    expect(typeof updater).toBe('function');
    expect(updater(sampleInteraction)).toBeNull();
    expect(updater({ ...sampleInteraction, interactionId: 'different' })).toEqual({
      ...sampleInteraction,
      interactionId: 'different',
    });
  });

  it('submits deny action with optional note when entered', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: sampleInteraction, setPendingInteraction }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    const noteInput = screen.getByPlaceholderText('Reason for approval or denial...');
    fireEvent.change(noteInput, { target: { value: '  dangerous command  ' } });

    fireEvent.click(screen.getByRole('button', { name: 'Deny' }));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith('test-agent', 'test-session', {
        interactionId: 'int-123',
        requestSeq: 5,
        response: {
          action: 'deny',
          data: {},
          note: 'dangerous command',
        },
      });
    });

    const updater = setPendingInteraction.mock.calls[0][0];
    expect(typeof updater).toBe('function');
    expect(updater(sampleInteraction)).toBeNull();
  });

  it('sets statusText when already_applied outcome is returned', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'already_applied' });
    const setStatusText = vi.fn();
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: sampleInteraction, setStatusText, setPendingInteraction }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    await waitFor(() => {
      expect(setStatusText).toHaveBeenCalledWith('This interaction was already resolved elsewhere.');
    });
    const updater = setPendingInteraction.mock.calls[0][0];
    expect(typeof updater).toBe('function');
    expect(updater(sampleInteraction)).toBeNull();
  });

  it('shows visible error on stale rejected result (error -32004)', async () => {
    const error = new InteractionResponseError('interaction response rejected', -32004, 'stale', { outcome: 'stale' });
    vi.mocked(submitInteractionResponse).mockRejectedValueOnce(error);
    const setErrorText = vi.fn();

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: sampleInteraction, setErrorText }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    await waitFor(() => {
      expect(screen.getByText(/stale/i)).toBeInTheDocument();
    });
    expect(setErrorText).toHaveBeenCalledWith(expect.stringMatching(/stale/i));
  });

  it('renders unsupported kind with "not supported by this client" and deny only', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });
    const unsupportedInteraction: PendingInteraction = {
      ...sampleInteraction,
      kind: 'wizard_form',
      message: 'Fill out this custom wizard',
    };

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: unsupportedInteraction }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByText('not supported by this client')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Deny' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Deny' }));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith('test-agent', 'test-session', {
        interactionId: 'int-123',
        requestSeq: 5,
        response: {
          action: 'deny',
          data: {},
          note: undefined,
        },
      });
    });
  });

  it('displays pending count when pendingCount > 0', () => {
    const multiInteraction: PendingInteraction = {
      ...sampleInteraction,
      pendingCount: 3,
    };

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, pendingInteraction: multiInteraction }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByText('3 more interactions awaiting response')).toBeInTheDocument();
  });

  it('queues interaction behind legacy approval UI so only one card is shown', async () => {
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([]);
    vi.mocked(submitHitlDecision).mockResolvedValueOnce({ applied: true });
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });

    const hydratedApprovals = [{ toolCallId: 'legacy-tool', summary: 'Approve legacy tool' }];
    const removeHydratedApproval = vi.fn();
    const setPendingInteraction = vi.fn();

    const { rerender } = render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          hydratedApprovals,
          removeHydratedApproval,
          pendingInteraction: sampleInteraction,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // Only legacy approval card is rendered
    expect(screen.getByText(/Approve legacy tool/)).toBeInTheDocument();
    expect(screen.queryByTestId('pending-interaction-card')).not.toBeInTheDocument();
    expect(screen.queryByText('Allow executing rm -rf?')).not.toBeInTheDocument();

    // Decide the legacy approval
    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    await waitFor(() => {
      expect(submitHitlDecision).toHaveBeenCalledWith('test-agent', 'test-session', {
        toolCallId: 'legacy-tool',
        approved: true,
        note: undefined,
      });
    });

    // When legacy approvals are cleared, interaction card appears
    rerender(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          hydratedApprovals: [],
          pendingInteraction: sampleInteraction,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByTestId('pending-interaction-card')).toBeInTheDocument();
    expect(screen.getByText('Allow executing rm -rf?')).toBeInTheDocument();
  });

  it('does not clear next interaction when event advances before RPC promise resolves', async () => {
    let resolveRpc: (val: any) => void = () => {};
    const rpcPromise = new Promise((resolve) => {
      resolveRpc = resolve;
    });
    vi.mocked(submitInteractionResponse).mockReturnValueOnce(rpcPromise as any);

    let currentPending: PendingInteraction | null = sampleInteraction;
    const setPendingInteraction = vi.fn((update: any) => {
      if (typeof update === 'function') {
        currentPending = update(currentPending);
      } else {
        currentPending = update;
      }
    });

    const { rerender } = render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: currentPending,
          setPendingInteraction,
          clearPendingInteractionIfMatch: (id, seq) => {
            if (currentPending?.interactionId === id && currentPending?.requestSeq === seq) {
              currentPending = null;
              setPendingInteraction(null);
            }
          },
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // Click Approve on int-123
    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));
    expect(submitInteractionResponse).toHaveBeenCalledTimes(1);

    // Server broadcasts next interaction before int-123 RPC resolves
    const nextInteraction: PendingInteraction = {
      interactionId: 'int-next-456',
      requestSeq: 6,
      toolCallId: 'tool-next',
      kind: 'confirm',
      message: 'Next interaction message',
      pendingCount: 0,
    };
    currentPending = nextInteraction;

    rerender(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: currentPending,
          setPendingInteraction,
          clearPendingInteractionIfMatch: (id, seq) => {
            if (currentPending?.interactionId === id && currentPending?.requestSeq === seq) {
              currentPending = null;
              setPendingInteraction(null);
            }
          },
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    expect(screen.getByText('Next interaction message')).toBeInTheDocument();

    // Now resolve the earlier RPC for int-123
    resolveRpc({ applied: true, outcome: 'applied' });
    await rpcPromise;

    // Next interaction must NOT be clobbered
    expect(currentPending).toEqual(nextInteraction);
    expect(screen.getByText('Next interaction message')).toBeInTheDocument();
  });

  it('handles identity change while submitting: resets submitting/note and does not leak errors from superseded call', async () => {
    let rejectRpc: (err: any) => void = () => {};
    const rpcPromise = new Promise((_, reject) => {
      rejectRpc = reject;
    });
    vi.mocked(submitInteractionResponse).mockReturnValueOnce(rpcPromise as any);

    let currentPending: PendingInteraction | null = sampleInteraction;

    const { rerender } = render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: currentPending,
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // Enter note and click Approve on int-123
    fireEvent.change(screen.getByPlaceholderText('Reason for approval or denial...'), {
      target: { value: 'note for first interaction' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    // Buttons should now be disabled
    expect(screen.getByRole('button', { name: 'Approve' })).toBeDisabled();

    // While submitting, interaction identity changes to next item
    const nextInteraction: PendingInteraction = {
      interactionId: 'int-789',
      requestSeq: 8,
      toolCallId: 'tool-789',
      kind: 'confirm',
      message: 'Interaction 789 message',
      pendingCount: 0,
    };
    currentPending = nextInteraction;

    rerender(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: currentPending,
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // Submitting state should be reset for new interaction
    expect(screen.getByText('Interaction 789 message')).toBeInTheDocument();
    const newApproveBtn = screen.getByRole('button', { name: 'Approve' });
    expect(newApproveBtn).not.toBeDisabled();
    // Note input should be cleared
    const noteInput = screen.getByPlaceholderText('Reason for approval or denial...') as HTMLInputElement;
    expect(noteInput.value).toBe('');

    // Now reject the superseded RPC call with stale error
    const staleError = new InteractionResponseError('interaction response rejected', -32004, 'stale', { outcome: 'stale' });
    rejectRpc(staleError);

    try {
      await rpcPromise;
    } catch {}

    // Error for superseded interaction must NOT appear on the new interaction
    expect(screen.queryByText(/stale/i)).not.toBeInTheDocument();
    expect(screen.getByText('Interaction 789 message')).toBeInTheDocument();
  });

  it('surfaces visible error and retains interaction card on malformed result', async () => {
    vi.mocked(submitInteractionResponse).mockRejectedValueOnce(
      new Error('RPC call returned malformed interaction response result')
    );
    const setErrorText = vi.fn();
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
          setErrorText,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    await waitFor(() => {
      expect(setErrorText).toHaveBeenCalledWith('RPC call returned malformed interaction response result');
    });

    expect(screen.getByRole('alert')).toHaveTextContent('RPC call returned malformed interaction response result');
    expect(screen.getByTestId('pending-interaction-card')).toBeInTheDocument();
    expect(setPendingInteraction).not.toHaveBeenCalled();
  });

  it('ignores late rejection after session switch and does not surface error on new session', async () => {
    let rejectRpc: (err: any) => void = () => {};
    const rpcPromise = new Promise((_, reject) => {
      rejectRpc = reject;
    });
    vi.mocked(submitInteractionResponse).mockReturnValueOnce(rpcPromise as any);

    const setErrorText = vi.fn();

    const { rerender } = render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
          setErrorText,
        }}
      >
        <InteractionUI
          agentName="agent-1"
          sessionId="session-1"
          interaction={sampleInteraction}
        />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));
    expect(submitInteractionResponse).toHaveBeenCalledTimes(1);

    // Switch session to session-2 before RPC finishes
    rerender(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
          setErrorText,
        }}
      >
        <InteractionUI
          agentName="agent-1"
          sessionId="session-2"
          interaction={sampleInteraction}
        />
      </PendingContext.Provider>
    );

    // Earlier request for session-1 rejects
    setErrorText.mockClear();
    rejectRpc(new Error('session-1 late failure'));
    try {
      await rpcPromise;
    } catch {}

    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(setErrorText).not.toHaveBeenCalled();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('ignores late success after session switch and does not clear new session interaction', async () => {
    let resolveRpc: (val: any) => void = () => {};
    const rpcPromise = new Promise((resolve) => {
      resolveRpc = resolve;
    });
    vi.mocked(submitInteractionResponse).mockReturnValueOnce(rpcPromise as any);

    const setStatusText = vi.fn();
    const onResolved = vi.fn();

    const { rerender } = render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
          setStatusText,
        }}
      >
        <InteractionUI
          agentName="agent-1"
          sessionId="session-1"
          interaction={sampleInteraction}
          onResolved={onResolved}
        />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    // Switch session to session-2 before RPC finishes
    rerender(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
          setStatusText,
        }}
      >
        <InteractionUI
          agentName="agent-1"
          sessionId="session-2"
          interaction={sampleInteraction}
          onResolved={onResolved}
        />
      </PendingContext.Provider>
    );

    resolveRpc({ applied: true, outcome: 'already_applied' });
    await rpcPromise;

    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(setStatusText).not.toHaveBeenCalled();
    expect(onResolved).not.toHaveBeenCalled();
  });

  it('ignores onPendingInteraction callback from obsolete agent after session switch', async () => {
    const capturedAgents: any[] = [];
    const origUseAgUiRuntime = agUi.useAgUiRuntime;
    const spy = vi.spyOn(agUi, 'useAgUiRuntime').mockImplementation((params: any) => {
      capturedAgents.push(params.agent);
      return origUseAgUiRuntime(params);
    });

    function InteractionViewer() {
      const { pendingInteraction } = useContext(PendingContext);
      return (
        <div data-testid="viewer-interaction">
          {pendingInteraction ? pendingInteraction.message : 'none'}
        </div>
      );
    }

    try {
      const { rerender } = render(
        <ChatProvider agentName="agent-a" sessionId="session-1" isFreshSession={false} onOpenSubAgent={vi.fn()}>
          <InteractionViewer />
        </ChatProvider>
      );

      expect(capturedAgents.length).toBeGreaterThan(0);
      const agent1 = capturedAgents[capturedAgents.length - 1];

      rerender(
        <ChatProvider agentName="agent-a" sessionId="session-2" isFreshSession={false} onOpenSubAgent={vi.fn()}>
          <InteractionViewer />
        </ChatProvider>
      );

      const agent2 = capturedAgents[capturedAgents.length - 1];
      expect(agent1).not.toBe(agent2);

      // Dispatch event on old agent (agent1)
      act(() => {
        (agent1 as any).handleAgentEvent({
          type: 'CUSTOM',
          name: 'pending_interaction',
          value: {
            interaction_id: 'int-old',
            request_seq: 1,
            tool_call_id: 'tc-old',
            kind: 'confirm',
            message: 'Old interaction message',
            pending_count: 0,
          },
        });
      });

      expect(screen.getByTestId('viewer-interaction')).toHaveTextContent('none');

      // Dispatch event on current agent (agent2)
      act(() => {
        (agent2 as any).handleAgentEvent({
          type: 'CUSTOM',
          name: 'pending_interaction',
          value: {
            interaction_id: 'int-new',
            request_seq: 1,
            tool_call_id: 'tc-new',
            kind: 'confirm',
            message: 'New interaction message',
            pending_count: 0,
          },
        });
      });

      expect(screen.getByTestId('viewer-interaction')).toHaveTextContent('New interaction message');
    } finally {
      spy.mockRestore();
    }
  });

  it('resets submitting state and note when scope changes (session changes) with identical interaction ID and seq', async () => {
    let resolveRpc: (val: any) => void = () => {};
    const rpcPromise = new Promise((resolve) => {
      resolveRpc = resolve;
    });
    vi.mocked(submitInteractionResponse).mockReturnValueOnce(rpcPromise as any);

    const { rerender } = render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
        }}
      >
        <InteractionUI
          agentName="agent-a"
          sessionId="session-1"
          interaction={sampleInteraction}
        />
      </PendingContext.Provider>
    );

    const noteInput = screen.getByPlaceholderText('Reason for approval or denial...');
    fireEvent.change(noteInput, { target: { value: 'note for session 1' } });
    fireEvent.click(screen.getByRole('button', { name: 'Approve' }));

    expect(screen.getByRole('button', { name: 'Approve' })).toBeDisabled();

    // Re-render same InteractionUI with new sessionId "session-2", but SAME interactionId and requestSeq
    rerender(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleInteraction,
        }}
      >
        <InteractionUI
          agentName="agent-a"
          sessionId="session-2"
          interaction={sampleInteraction}
        />
      </PendingContext.Provider>
    );

    // Submitting state must be reset, Approve button enabled, note input cleared
    expect(screen.getByRole('button', { name: 'Approve' })).toBeEnabled();
    const newNoteInput = screen.getByPlaceholderText('Reason for approval or denial...') as HTMLInputElement;
    expect(newNoteInput.value).toBe('');

    // Now earlier request finishes; must not impact session-2
    resolveRpc({ applied: true, outcome: 'applied' });
    await rpcPromise;

    expect(screen.getByRole('button', { name: 'Approve' })).toBeEnabled();
  });

  it('delivers pending_interaction event correctly when ChatProvider is wrapped in StrictMode', async () => {
    let capturedAgent: any;
    const origUseAgUiRuntime = agUi.useAgUiRuntime;
    const spy = vi.spyOn(agUi, 'useAgUiRuntime').mockImplementation((params: any) => {
      capturedAgent = params.agent;
      return origUseAgUiRuntime(params);
    });

    function InteractionViewer() {
      const { pendingInteraction } = useContext(PendingContext);
      return (
        <div data-testid="strict-viewer-interaction">
          {pendingInteraction ? pendingInteraction.message : 'none'}
        </div>
      );
    }

    try {
      render(
        <StrictMode>
          <ChatProvider agentName="agent-strict" sessionId="session-strict" isFreshSession={false} onOpenSubAgent={vi.fn()}>
            <InteractionViewer />
          </ChatProvider>
        </StrictMode>
      );

      expect(capturedAgent).toBeDefined();

      // Dispatch pending_interaction event on the agent
      act(() => {
        (capturedAgent as any).handleAgentEvent({
          type: 'CUSTOM',
          name: 'pending_interaction',
          value: {
            interaction_id: 'int-strict',
            request_seq: 1,
            tool_call_id: 'tc-strict',
            kind: 'confirm',
            message: 'Strict mode interaction',
            pending_count: 0,
          },
        });
      });

      // After StrictMode setup->cleanup->setup, the event must NOT be ignored
      expect(screen.getByTestId('strict-viewer-interaction')).toHaveTextContent('Strict mode interaction');
    } finally {
      spy.mockRestore();
    }
  });

    it('clears pending interaction on session switch in ChatProvider', async () => {
    function InteractionViewer() {
      const { pendingInteraction, setPendingInteraction } = useContext(PendingContext);
      return (
        <div>
          <button
            type="button"
            onClick={() => setPendingInteraction?.(sampleInteraction)}
          >
            Set Pending
          </button>
          <div data-testid="viewer-interaction">
            {pendingInteraction ? pendingInteraction.message : 'none'}
          </div>
        </div>
      );
    }

    const { rerender } = render(
      <ChatProvider agentName="agent-a" sessionId="session-1" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <InteractionViewer />
      </ChatProvider>
    );

    expect(screen.getByTestId('viewer-interaction')).toHaveTextContent('none');

    // Set pending interaction
    fireEvent.click(screen.getByRole('button', { name: 'Set Pending' }));
    expect(screen.getByTestId('viewer-interaction')).toHaveTextContent('Allow executing rm -rf?');

    // Switch session
    rerender(
      <ChatProvider agentName="agent-a" sessionId="session-2" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <InteractionViewer />
      </ChatProvider>
    );

    // Should be cleared to none on session switch
    expect(screen.getByTestId('viewer-interaction')).toHaveTextContent('none');
  });

  it('does not let legacy RUN_FINISHED interrupt send old hitl RPC for interactions', () => {
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([
      { id: 'int-legacy-outcome', toolCallId: sampleInteraction.toolCallId, reason: 'tool_call', message: 'Legacy outcome' } as any,
    ]);

    render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          hydratedApprovals: [],
          pendingInteraction: sampleInteraction,
        }}
      >
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    // Should render the interaction card, NOT the legacy approval card
    expect(screen.getByTestId('pending-interaction-card')).toBeInTheDocument();
    expect(screen.queryByTestId('hydrated-pending-approval')).not.toBeInTheDocument();
    expect(screen.queryByText('Legacy outcome')).not.toBeInTheDocument();
  });
});

describe('status announcements', () => {
  it('keeps status notices in a polite atomic live region', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({ thread: { isRunning: false } }),
    );

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, statusText: 'This approval was already resolved elsewhere.' }}>
        <UsageContext.Provider value={{ usage: null, toolSummaries: new Map(), toolUpdates: new Map() }}>
          <StatusBar />
        </UsageContext.Provider>
      </PendingContext.Provider>,
    );

    expect(screen.getByRole('status')).toHaveAttribute('aria-live', 'polite');
    expect(screen.getByRole('status')).toHaveAttribute('aria-atomic', 'true');
    expect(screen.getByText('This approval was already resolved elsewhere.')).toBeVisible();
  });

  it('announces send errors assertively', () => {
    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, errorText: 'The decision could not be sent.' }}>
        <SendErrorIndicator />
      </PendingContext.Provider>,
    );

    expect(screen.getByRole('alert')).toHaveAttribute('aria-live', 'assertive');
  });
});

describe('MyComposer', () => {
  let composerRuntime: any;
  let setErrorText: any;
  let markSessionNotFresh: any;

  beforeEach(() => {
    vi.clearAllMocks();
    setErrorText = vi.fn();
    markSessionNotFresh = vi.fn();
    vi.mocked(useCancellation).mockReturnValue(cancellationControl('idle'));

    // Default useAuiState mock (not running, 0 user messages)
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) => {
      const state = { thread: { isRunning: false, messages: [] } };
      return selector(state);
    });

    composerRuntime = {
      getState: () => ({ text: 'hello', attachments: [] }),
      setText: vi.fn(),
      clearAttachments: vi.fn(),
      addAttachment: vi.fn(),
      send: vi.fn(),
      subscribe: vi.fn(() => () => {}),
    };
    vi.mocked(aui.useAui).mockReturnValue({ 
      composer: composerRuntime,
      thread: { getState: () => ({ isRunning: false }) }
    } as any);
  });

  const renderComposer = (isFreshSession: boolean) => {
    return render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={isFreshSession} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
          <MyComposer 
            agentName="foo" 
            sessionId="bar" 
            isFreshSession={isFreshSession} 
            markSessionNotFresh={markSessionNotFresh}
            switchAgentHref="" 
            switchSessionHref="" 
            onSwitchAgent={() => {}} 
            onSwitchSession={() => {}} 
          />
        </PendingContext.Provider>
      </ChatProvider>
    );
  };

  it('routes fresh session submit via streaming path and marks not fresh', async () => {
    renderComposer(true);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    expect(composerRuntime.send).toHaveBeenCalled();
    expect(markSessionNotFresh).toHaveBeenCalledWith('bar');
    expect(sendPrompt).not.toHaveBeenCalled();
  });

  it('blocks composer controls and submission while the interrupt is being appended', () => {
    vi.mocked(useCancellation).mockReturnValue(cancellationControl('requesting'));
    renderComposer(false);

    expect(screen.getByRole('textbox')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Attach file' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Send' })).toBeDisabled();
    fireEvent.submit(document.querySelector('form')!);
    expect(sendPrompt).not.toHaveBeenCalled();
    expect(composerRuntime.send).not.toHaveBeenCalled();
  });

  it('routes existing session submit via sendPrompt (no second run)', async () => {
    // This guards the "no duplicate turn" invariant: an out-of-band send must NOT trigger the streaming runAgent path.
    // The server-side pending_user_prompt behavior is already covered by Rust tests (ag_ui_tests.rs).
    vi.mocked(sendPrompt).mockResolvedValueOnce({ status: 'enqueued', run_id: '1', message_id: 'server-1' });
    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    expect(composerRuntime.send).not.toHaveBeenCalled();
    await waitFor(() => expect(sendPrompt).toHaveBeenCalledWith('foo', 'bar', { text: 'hello', attachmentRefs: [] }));
  });

  it('routes message #2 via sendPrompt after fresh session flag is cleared', async () => {
    const { rerender } = renderComposer(true);
    const form = document.querySelector('form');
    fireEvent.submit(form!);
    expect(composerRuntime.send).toHaveBeenCalled();
    expect(markSessionNotFresh).toHaveBeenCalledWith('bar');
    vi.mocked(composerRuntime.send).mockClear();

    // Rerender as existing session
    rerender(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
          <MyComposer 
            agentName="foo" 
            sessionId="bar" 
            isFreshSession={false} 
            markSessionNotFresh={markSessionNotFresh}
            switchAgentHref="" 
            switchSessionHref="" 
            onSwitchAgent={() => {}} 
            onSwitchSession={() => {}} 
          />
        </PendingContext.Provider>
      </ChatProvider>
    );

    fireEvent.submit(form!);
    expect(composerRuntime.send).not.toHaveBeenCalled();
    await waitFor(() => expect(sendPrompt).toHaveBeenCalled());
  });

  it('blocks another submit only until the queued prompt is acknowledged', async () => {
    let resolveSendPrompt: (value: any) => void;
    vi.mocked(sendPrompt).mockReturnValue(new Promise(resolve => {
      resolveSendPrompt = resolve;
    }));

    const onPromptQueued = vi.fn();
    render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <MyComposer agentName="foo" sessionId="bar" isFreshSession={false}
          markSessionNotFresh={markSessionNotFresh} onSwitchAgent={() => {}}
          onSwitchSession={() => {}} switchAgentHref="" switchSessionHref=""
          onPromptQueued={onPromptQueued} />
      </ChatProvider>,
    );
    const form = document.querySelector('form');
    
    // First submit
    fireEvent.submit(form!);
    
    // Composer disabled
    expect(screen.getByRole('textbox')).toBeDisabled();
    expect(document.querySelector('.aui-spinner')).toBeInTheDocument();

    // Draft is NOT cleared while send is in-flight
    expect(composerRuntime.setText).not.toHaveBeenCalled();
    expect(composerRuntime.clearAttachments).not.toHaveBeenCalled();
    
    // Try second submit
    fireEvent.submit(form!);
    await waitFor(() => expect(sendPrompt).toHaveBeenCalledTimes(1));
    expect(composerRuntime.setText).not.toHaveBeenCalled();
    expect(composerRuntime.clearAttachments).not.toHaveBeenCalled();
    
    // Resolve RPC
    await act(async () => resolveSendPrompt!({ status: 'enqueued', run_id: '1', message_id: 'server-1' }));

    // Composer IS cleared after sendPrompt resolves
    await waitFor(() => {
      expect(composerRuntime.setText).toHaveBeenCalledWith('');
    });
    expect(composerRuntime.clearAttachments).toHaveBeenCalled();
    
    await waitFor(() => expect(screen.getByRole('textbox')).not.toBeDisabled());
    expect(onPromptQueued).toHaveBeenCalledWith(expect.objectContaining({
      text: 'hello', attachmentNames: [],
    }));
    expect(document.querySelector('.aui-spinner')).not.toBeInTheDocument();

    fireEvent.submit(form!);
    await waitFor(() => expect(sendPrompt).toHaveBeenCalledTimes(2));
  });

  it('shows accepted prompts from a legacy response without a message ID', async () => {
    vi.mocked(sendPrompt).mockResolvedValueOnce({ status: 'enqueued', run_id: '1', message_id: null });
    const onPromptQueued = vi.fn();
    render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <MyComposer agentName="foo" sessionId="bar" isFreshSession={false}
          markSessionNotFresh={markSessionNotFresh} onSwitchAgent={() => {}}
          onSwitchSession={() => {}} switchAgentHref="" switchSessionHref=""
          onPromptQueued={onPromptQueued} />
      </ChatProvider>,
    );
    fireEvent.submit(document.querySelector('form')!);
    await waitFor(() => expect(onPromptQueued).toHaveBeenCalledWith(expect.objectContaining({
      text: 'hello', messageId: null,
    })));
    await waitFor(() => expect(screen.getByRole('textbox')).toBeEnabled());
  });

  it('keeps an accepted prompt queued when local composer cleanup fails', async () => {
    vi.mocked(sendPrompt).mockResolvedValueOnce({ status: 'enqueued', run_id: '1', message_id: 'server-1' });
    vi.mocked(uploadAttachment).mockResolvedValueOnce(['cid:file.png']);
    composerRuntime.clearAttachments.mockRejectedValueOnce(new Error('clear failed'));
    let composerAttachments = [{ id: 'retained-file', name: 'file.png', file: new File(['data'], 'file.png') }];
    composerRuntime.getState = () => ({ text: 'hello', attachments: composerAttachments });
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) => selector({
      thread: { isRunning: false, messages: [] },
      composer: { attachments: composerAttachments },
    }));
    const onPromptQueued = vi.fn();
    const composer = () => (
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
          <MyComposer agentName="foo" sessionId="bar" isFreshSession={false}
            markSessionNotFresh={markSessionNotFresh} onSwitchAgent={() => {}}
            onSwitchSession={() => {}} switchAgentHref="" switchSessionHref=""
            onPromptQueued={onPromptQueued} />
        </PendingContext.Provider>
      </ChatProvider>
    );
    const { rerender } = render(composer());
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    try {
      fireEvent.submit(document.querySelector('form')!);
      await waitFor(() => expect(onPromptQueued).toHaveBeenCalledWith(expect.objectContaining({ text: 'hello' })));
      await waitFor(() => expect(screen.getByRole('button', { name: 'Send' })).toBeDisabled());
      expect(setErrorText).toHaveBeenCalledWith(expect.stringContaining('Message queued'));
      fireEvent.submit(document.querySelector('form')!);
      expect(sendPrompt).toHaveBeenCalledTimes(1);

      composerAttachments = [];
      rerender(composer());
      expect(screen.getByRole('button', { name: 'Send' })).toBeEnabled();
    } finally {
      consoleError.mockRestore();
    }
  });

  const submitWithSendPromptError = async (error: unknown) => {
    vi.mocked(uploadAttachment).mockResolvedValueOnce(['cid:file.png']);
    vi.mocked(sendPrompt).mockRejectedValueOnce(error);

    const file = new File([''], 'file.png');
    composerRuntime.getState = () => ({ text: 'my draft', attachments: [{ status: { type: 'running' }, file }] });

    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    await waitFor(() => {
      expect(screen.getByRole('textbox')).not.toBeDisabled();
    });

    expect(composerRuntime.setText).not.toHaveBeenCalled();
    expect(composerRuntime.clearAttachments).not.toHaveBeenCalled();
    expect(composerRuntime.addAttachment).not.toHaveBeenCalled();
  };

  it('retains input text and attachments on sendPrompt error', async () => {
    await submitWithSendPromptError(new Error('RPC boom'));
    expect(setErrorText).toHaveBeenCalledWith('RPC boom');
  });

  it('retains input text and attachments on sendPrompt timeout without surfacing error (#1861)', async () => {
    await submitWithSendPromptError(
      new DOMException('The operation was aborted due to timeout', 'TimeoutError')
    );
    expect(setErrorText).not.toHaveBeenCalledWith(expect.any(String));
  });

  it('retains input text and attachments on bare abort ("signal is aborted without reason") without surfacing error (#1838)', async () => {
    await submitWithSendPromptError(
      new DOMException('signal is aborted without reason', 'AbortError')
    );
    expect(setErrorText).not.toHaveBeenCalledWith(expect.any(String));
  });

  it('keeps text and attachments visible while sendPrompt is pending and clears only on success', async () => {
    let resolveSendPrompt: (value: any) => void;
    vi.mocked(sendPrompt).mockReturnValue(new Promise(resolve => {
      resolveSendPrompt = resolve;
    }));
    vi.mocked(uploadAttachment).mockResolvedValueOnce(['cid:file.png']);

    const file = new File([''], 'file.png');
    const attachment = { status: { type: 'running' as const }, file, type: 'image' };
    composerRuntime.getState = () => ({
      text: 'pending draft message',
      attachments: [attachment],
    });

    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    await waitFor(() => {
      expect(uploadAttachment).toHaveBeenCalledWith('foo', 'bar', file);
    });

    expect(sendPrompt).toHaveBeenCalledWith('foo', 'bar', {
      text: 'pending draft message',
      attachmentRefs: ['cid:file.png'],
    });

    // In-flight window: input is disabled and spinner shown
    expect(screen.getByRole('textbox')).toBeDisabled();
    expect(document.querySelector('.aui-spinner')).toBeInTheDocument();

    // Draft is NOT cleared yet (setText and clearAttachments have not been called)
    expect(composerRuntime.setText).not.toHaveBeenCalled();
    expect(composerRuntime.clearAttachments).not.toHaveBeenCalled();

    // Resolve sendPrompt
    resolveSendPrompt!({ status: 'enqueued', run_id: '1' });

    // After success: composer is cleared
    await waitFor(() => {
      expect(composerRuntime.setText).toHaveBeenCalledWith('');
    });
    expect(composerRuntime.clearAttachments).toHaveBeenCalled();
  });

  it('uploads attachments on existing-session submit', async () => {
    vi.mocked(sendPrompt).mockResolvedValueOnce({ status: 'enqueued', run_id: '1', message_id: 'server-1' });
    vi.mocked(uploadAttachment).mockResolvedValueOnce(['cid:file1']);
    
    const file = new File([''], 'test.png');
    composerRuntime.getState = () => ({ 
      text: 'hello', 
      attachments: [{ status: { type: 'running' }, file, type: 'image' }] 
    });

    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    await waitFor(() => {
      expect(uploadAttachment).toHaveBeenCalledWith('foo', 'bar', file);
    });

    expect(sendPrompt).toHaveBeenCalledWith('foo', 'bar', { 
      text: 'hello', 
      attachmentRefs: ['cid:file1'] 
    });
  });

  it('does not mark unread session as read on focus alone, but marks on input or submit once per unread state', async () => {
    const onMarkRead = vi.fn();
    const { rerender } = render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
          <MyComposer 
            agentName="foo" 
            sessionId="bar" 
            isUnread={true}
            onMarkRead={onMarkRead}
            isFreshSession={false} 
            markSessionNotFresh={markSessionNotFresh}
            switchAgentHref="" 
            switchSessionHref="" 
            onSwitchAgent={() => {}} 
            onSwitchSession={() => {}} 
          />
        </PendingContext.Provider>
      </ChatProvider>
    );

    const textarea = screen.getByRole('textbox');
    
    // Focus alone must NOT mark read (viewport/focus auto-read is deferred)
    fireEvent.focus(textarea);
    expect(markRead).not.toHaveBeenCalled();
    expect(onMarkRead).not.toHaveBeenCalled();

    // First keystroke marks read
    fireEvent.input(textarea, { target: { value: 'a' } });

    await waitFor(() => {
      expect(markRead).toHaveBeenCalledWith('foo', 'bar');
      expect(onMarkRead).toHaveBeenCalledTimes(1);
    });

    // Subsequent keystrokes should not call markRead again
    fireEvent.input(textarea, { target: { value: 'ab' } });
    expect(markRead).toHaveBeenCalledTimes(1);

    // If rerendered after becoming read (isUnread: false), focusing or typing does not call markRead
    rerender(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
          <MyComposer 
            agentName="foo" 
            sessionId="bar" 
            isUnread={false}
            onMarkRead={onMarkRead}
            isFreshSession={false} 
            markSessionNotFresh={markSessionNotFresh}
            switchAgentHref="" 
            switchSessionHref="" 
            onSwitchAgent={() => {}} 
            onSwitchSession={() => {}} 
          />
        </PendingContext.Provider>
      </ChatProvider>
    );

    fireEvent.focus(textarea);
    fireEvent.input(textarea, { target: { value: 'abc' } });
    expect(markRead).toHaveBeenCalledTimes(1);
  });

  it('marks unread session as read on submit without prior input', async () => {
    const onMarkRead = vi.fn();
    render(
      <ChatProvider agentName="foo" sessionId="bar-submit" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
          <MyComposer 
            agentName="foo" 
            sessionId="bar-submit" 
            isUnread={true}
            onMarkRead={onMarkRead}
            isFreshSession={false} 
            markSessionNotFresh={markSessionNotFresh}
            switchAgentHref="" 
            switchSessionHref="" 
            onSwitchAgent={() => {}} 
            onSwitchSession={() => {}} 
          />
        </PendingContext.Provider>
      </ChatProvider>
    );

    const sendBtn = screen.getByRole('button', { name: 'Send' });
    fireEvent.click(sendBtn);

    await waitFor(() => {
      expect(markRead).toHaveBeenCalledWith('foo', 'bar-submit');
      expect(onMarkRead).toHaveBeenCalledTimes(1);
    });
  });

  it('renders attachment chip with horizontal layout, thumbnail, filename, and remove button', () => {
    const { container } = render(<MyAttachment />);
    const chip = container.querySelector('.aui-attachment');
    expect(chip).toBeInTheDocument();
    expect(container.querySelector('.aui-attachment-thumb')).toBeInTheDocument();
    expect(container.querySelector('.aui-attachment-info')).toBeInTheDocument();
    expect(container.querySelector('.aui-attachment-name')).toBeInTheDocument();
    expect(screen.getByText('diagram.png')).toBeInTheDocument();
    const removeBtn = container.querySelector('.aui-attachment-remove');
    expect(removeBtn).toBeInTheDocument();
    expect(removeBtn).toHaveAttribute('aria-label', 'Remove attachment');
  });
});

describe('MarkdownLink', () => {
  it('renders markdown links with target="_blank" and rel="noopener noreferrer"', () => {
    render(<MarkdownLink href="https://example.com">Documentation</MarkdownLink>);
    const link = screen.getByRole('link', { name: 'Documentation' });
    expect(link).toHaveAttribute('href', 'https://example.com');
    expect(link).toHaveAttribute('target', '_blank');
    expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  });

  it('stops click propagation so parent accordions are not triggered', () => {
    const parentClick = vi.fn();
    render(
      <div onClick={parentClick}>
        <MarkdownLink href="https://example.com">External Resource</MarkdownLink>
      </div>
    );
    fireEvent.click(screen.getByRole('link', { name: 'External Resource' }));
    expect(parentClick).not.toHaveBeenCalled();
  });
});

describe('MyMessage attachment rendering', () => {
  it('renders image attachment on attachment-only (empty text) user row', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({
        message: {
          id: 'user-msg-1',
          role: 'user',
          content: [],
        },
      }),
    );

    render(
      <MessageAttachmentsContext.Provider
        value={{
          agent: 'agent-1',
          session: 'session-1',
          attachmentsByMessageId: {
            'user-msg-1': [{ partIndex: 0, cid: 'cid:img123', kind: 'image' }],
          },
        }}
      >
        <MyMessage />
      </MessageAttachmentsContext.Provider>,
    );

    const img = screen.getByRole('img');
    expect(img).toBeInTheDocument();
    expect(img).toHaveAttribute(
      'src',
      '/v1/agents/agent-1/sessions/session-1/attachments/cid%3Aimg123',
    );
  });

  it('renders image attachment alongside text on user message', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({
        message: {
          id: 'user-msg-2',
          role: 'user',
          content: [{ type: 'text', text: 'Check this image' }],
        },
      }),
    );

    render(
      <MessageAttachmentsContext.Provider
        value={{
          agent: 'agent-1',
          session: 'session-1',
          attachmentsByMessageId: {
            'user-msg-2': [{ partIndex: 0, cid: 'cid:img456', kind: 'image' }],
          },
        }}
      >
        <MyMessage />
      </MessageAttachmentsContext.Provider>,
    );

    const img = screen.getByRole('img');
    expect(img).toBeInTheDocument();
    expect(img).toHaveAttribute(
      'src',
      '/v1/agents/agent-1/sessions/session-1/attachments/cid%3Aimg456',
    );
  });
});

describe('MyMessage compaction marker rendering', () => {
  const compaction = {
    message_id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
    compaction_seq: 5,
    summary: 'The session log stores tool calls and results separately. No messages were kept.',
    from_seq: 1,
    to_seq: 4,
    kept_count: 0,
  };

  it('renders synthetic assistant message as a compaction marker when message.id === compaction.message_id', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({
        message: {
          id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
          role: 'assistant',
          content: [
            {
              type: 'text',
              text: 'The session log stores tool calls and results separately. No messages were kept.',
            },
          ],
        },
      }),
    );

    render(
      <CompactionMetadataContext.Provider value={compaction}>
        <MyMessage />
      </CompactionMetadataContext.Provider>,
    );

    expect(screen.getByTestId('compaction-marker')).toBeInTheDocument();
    expect(screen.getByTestId('compaction-summary')).toBeInTheDocument();
    expect(screen.getByText('Compaction')).toBeInTheDocument();
    expect(screen.getByText('Summary of earlier conversation')).toBeInTheDocument();
    expect(screen.getByText('Earlier history archived')).toBeInTheDocument();
  });

  it('renders normal assistant reply when message.id !== compaction.message_id', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({
        message: {
          id: 'regular-assistant-reply',
          role: 'assistant',
          content: [{ type: 'text', text: 'Normal assistant response' }],
        },
      }),
    );

    const { container } = render(
      <CompactionMetadataContext.Provider value={compaction}>
        <MyMessage />
      </CompactionMetadataContext.Provider>,
    );

    expect(screen.queryByTestId('compaction-marker')).not.toBeInTheDocument();
    expect(container.querySelector('.aui-assistant-message')).toBeInTheDocument();
  });

  it('renders normal assistant reply when compaction is null', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({
        message: {
          id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
          role: 'assistant',
          content: [{ type: 'text', text: 'Some text' }],
        },
      }),
    );

    const { container } = render(
      <CompactionMetadataContext.Provider value={null}>
        <MyMessage />
      </CompactionMetadataContext.Provider>,
    );

    expect(screen.queryByTestId('compaction-marker')).not.toBeInTheDocument();
    expect(container.querySelector('.aui-assistant-message')).toBeInTheDocument();
  });
});
