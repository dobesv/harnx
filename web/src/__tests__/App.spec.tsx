class MockEventSource {
  close() {}
  addEventListener() {}
}
(globalThis as any).EventSource = MockEventSource;

import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import {
  BatchInterruptUI,
  CancelButton,
  MyComposer,
  SendErrorIndicator,
  StatusBar,
  StatusIndicator,
} from '../App';
import { sendPrompt, uploadAttachment, submitHitlDecision } from '../api';
import { PendingContext } from '../PendingContext';
import { UsageContext } from '../UsageContext';
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
  hydratedApprovals: [],
  addHydratedApproval: vi.fn(),
  clearHydratedApprovals: vi.fn(), removeHydratedApproval: vi.fn(),
};

vi.mock('@assistant-ui/react-ag-ui', async (importOriginal) => {
  const actual = await importOriginal<typeof agUi>();
  return { ...actual, useAgUiInterrupts: vi.fn(), useAgUiSubmitInterruptResponses: vi.fn() };
});

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return { ...actual, sendPrompt: vi.fn(), uploadAttachment: vi.fn(), submitHitlDecision: vi.fn() };
});
vi.mock('../useCancellation', () => ({ useCancellation: vi.fn() }));
vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return { 
    ...actual, 
    useAuiState: vi.fn(), 
    useAui: vi.fn(),
  };
});

const cancellationControl = (
  phase: CancellationControl['phase'],
  stop = vi.fn(async () => {}),
): CancellationControl => ({ phase, stop, observe: vi.fn() });

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
    fireEvent.click(screen.getByRole('button', { name: 'Stop' }));
    expect(stop).toHaveBeenCalledOnce();
  });

  it('disables cancellation while the request is being accepted or work is stopping', () => {
    const { rerender } = render(
      <CancellationContext.Provider value={cancellationControl('requesting')}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    expect(screen.getByRole('button', { name: 'Requesting cancellation…' })).toBeDisabled();

    rerender(
      <CancellationContext.Provider value={cancellationControl('stopping')}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    expect(screen.getByRole('button', { name: 'Stopping…' })).toBeDisabled();
  });

  it('offers retry after cancellation becomes unconfirmed or the request fails', () => {
    const stop = vi.fn(async () => {});
    const { rerender } = render(
      <CancellationContext.Provider value={cancellationControl('unconfirmed', stop)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    const unconfirmed = screen.getByRole('button', { name: 'Cancellation unconfirmed — Retry' });
    expect(unconfirmed).toBeEnabled();
    fireEvent.click(unconfirmed);

    rerender(
      <CancellationContext.Provider value={cancellationControl('failed', stop)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    const failed = screen.getByRole('button', { name: 'Cancellation request failed — Retry' });
    expect(failed).toBeEnabled();
    fireEvent.click(failed);
    expect(stop).toHaveBeenCalledTimes(2);
  });

  it('uses a static warning instead of a spinner when cancellation is unconfirmed', () => {
    const { rerender, container } = render(
      <CancellationContext.Provider value={cancellationControl('stopping')}>
        <StatusIndicator isRunning statusText={null} />
      </CancellationContext.Provider>,
    );
    expect(screen.getByText('Cancelling')).toBeInTheDocument();
    expect(container.querySelector('.aui-spinner')).toBeInTheDocument();

    rerender(
      <CancellationContext.Provider value={cancellationControl('unconfirmed')}>
        <StatusIndicator isRunning statusText={null} />
      </CancellationContext.Provider>,
    );
    expect(screen.getByText('Cancellation unconfirmed')).toBeInTheDocument();
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

  it('surfaces an error on rejection', async () => {
    const setErrorText = vi.fn();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-2', toolCallId: 'tool-2', reason: '' } as any]);
    vi.mocked(submitHitlDecision).mockRejectedValue(new Error('Network error'));

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, setErrorText }}>
        <BatchInterruptUI agentName="test-agent" sessionId="test-session" />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByText('Approve'));

    await waitFor(() => {
      expect(setErrorText).toHaveBeenCalledWith('Network error');
    });
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

describe('status announcements', () => {
  it('keeps status notices in a polite atomic live region', () => {
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
      selector({ thread: { isRunning: false } }),
    );

    render(
      <PendingContext.Provider value={{ ...defaultPendingContext, statusText: 'This approval was already resolved elsewhere.' }}>
        <UsageContext.Provider value={{ usage: null, toolSummaries: new Map() }}>
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

  it('blocks composer controls and submission throughout cancellation', () => {
    vi.mocked(useCancellation).mockReturnValue(cancellationControl('stopping'));
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
    vi.mocked(sendPrompt).mockResolvedValueOnce({ status: 'enqueued', run_id: '1' });
    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    expect(composerRuntime.send).not.toHaveBeenCalled();
    expect(sendPrompt).toHaveBeenCalledWith('foo', 'bar', { text: 'hello', attachmentRefs: [] });
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
    expect(sendPrompt).toHaveBeenCalled();
  });

  it('shows busy state on submit, blocks second submit, and clears when message lands', async () => {
    let resolveSendPrompt: (value: any) => void;
    vi.mocked(sendPrompt).mockReturnValue(new Promise(resolve => {
      resolveSendPrompt = resolve;
    }));

    let userMessages = 0;
    vi.mocked(aui.useAuiState).mockImplementation((selector: any) => {
      const state = { thread: { isRunning: false, messages: Array(userMessages).fill({ role: 'user' }) } };
      return selector(state);
    });

    const { rerender } = renderComposer(false);
    const form = document.querySelector('form');
    
    // First submit
    fireEvent.submit(form!);
    
    // Composer disabled
    expect(screen.getByRole('textbox')).toBeDisabled();
    expect(document.querySelector('.aui-spinner')).toBeInTheDocument();
    
    // Try second submit
    fireEvent.submit(form!);
    expect(sendPrompt).toHaveBeenCalledTimes(1);
    
    // Resolve RPC
    resolveSendPrompt!({ status: 'enqueued', run_id: '1' });
    
    // Still sending because user message count hasn't increased
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
    expect(screen.getByRole('textbox')).toBeDisabled();

    // Simulate message landing in transcript
    userMessages = 1;
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

    // No longer sending
    expect(screen.getByRole('textbox')).not.toBeDisabled();
    expect(document.querySelector('.aui-spinner')).not.toBeInTheDocument();
  });

  it('restores input text and attachments on sendPrompt error', async () => {
    vi.mocked(uploadAttachment).mockResolvedValueOnce(['cid:file.png']);
    vi.mocked(sendPrompt).mockRejectedValueOnce(new Error('RPC boom'));
    
    const file = new File([''], 'file.png');
    composerRuntime.getState = () => ({ text: 'my draft', attachments: [{ status: { type: 'running' }, file }] });

    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    await waitFor(() => {
      expect(setErrorText).toHaveBeenCalledWith('RPC boom');
    });

    expect(composerRuntime.setText).toHaveBeenCalledWith('my draft');
    expect(composerRuntime.addAttachment).toHaveBeenCalledWith(file);
    // Re-enables input
    expect(screen.getByRole('textbox')).not.toBeDisabled();
  });

  it('uploads attachments on existing-session submit', async () => {
    vi.mocked(sendPrompt).mockResolvedValueOnce({ status: 'enqueued', run_id: '1' });
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
});
