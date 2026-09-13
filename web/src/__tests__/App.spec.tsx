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
  SessionPicker,
  StatusBar,
  StatusIndicator,
} from '../App';
import { sendPrompt, uploadAttachment, submitHitlDecision, markRead } from '../api';
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
  return {
    ...actual,
    sendPrompt: vi.fn(),
    uploadAttachment: vi.fn(),
    submitHitlDecision: vi.fn(),
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
  };
});

const cancellationControl = (
  phase: CancellationControl['phase'],
  stop = vi.fn(async () => {}),
  resumeAnyway = vi.fn(async () => {}),
): CancellationControl => ({ phase, stop, resumeAnyway, observe: vi.fn() });

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

  it('offers retry and confirmed resume after cancellation becomes unconfirmed', () => {
    const stop = vi.fn(async () => {});
    const resumeAnyway = vi.fn(async () => {});
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(
      <CancellationContext.Provider value={cancellationControl('unconfirmed', stop, resumeAnyway)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Retry cancellation' }));
    fireEvent.click(screen.getByRole('button', { name: 'Resume anyway' }));
    expect(stop).toHaveBeenCalledOnce();
    expect(confirm).toHaveBeenCalledWith('Resume anyway? Prior work may still be running.');
    expect(resumeAnyway).toHaveBeenCalledOnce();
    confirm.mockRestore();
  });

  it('offers retry after the cancellation request fails', () => {
    const stop = vi.fn(async () => {});
    render(
      <CancellationContext.Provider value={cancellationControl('failed', stop)}>
        <CancelButton />
      </CancellationContext.Provider>,
    );
    const failed = screen.getByRole('button', { name: 'Cancellation request failed — Retry' });
    expect(failed).toBeEnabled();
    fireEvent.click(failed);
    expect(stop).toHaveBeenCalledOnce();
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

  const submitWithSendPromptError = async (error: unknown) => {
    vi.mocked(uploadAttachment).mockResolvedValueOnce(['cid:file.png']);
    vi.mocked(sendPrompt).mockRejectedValueOnce(error);

    const file = new File([''], 'file.png');
    composerRuntime.getState = () => ({ text: 'my draft', attachments: [{ status: { type: 'running' }, file }] });

    renderComposer(false);
    const form = document.querySelector('form');
    fireEvent.submit(form!);

    await waitFor(() => {
      expect(composerRuntime.setText).toHaveBeenCalledWith('my draft');
    });

    expect(composerRuntime.addAttachment).toHaveBeenCalledWith(file);
    expect(screen.getByRole('textbox')).not.toBeDisabled();
  };

  it('restores input text and attachments on sendPrompt error', async () => {
    await submitWithSendPromptError(new Error('RPC boom'));
    expect(setErrorText).toHaveBeenCalledWith('RPC boom');
  });

  it('restores input text and attachments on sendPrompt timeout without surfacing error (#1861)', async () => {
    await submitWithSendPromptError(
      new DOMException('The operation was aborted due to timeout', 'TimeoutError')
    );
    expect(setErrorText).not.toHaveBeenCalledWith(expect.any(String));
  });

  it('restores input text and attachments on bare abort ("signal is aborted without reason") without surfacing error (#1838)', async () => {
    await submitWithSendPromptError(
      new DOMException('signal is aborted without reason', 'AbortError')
    );
    expect(setErrorText).not.toHaveBeenCalledWith(expect.any(String));
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
});

describe('SessionPicker', () => {
  const sessions = [
    { session_id: 'session-old-read', updated_at: '2026-01-01T00:00:00Z', unread: false },
    { session_id: 'session-new-read', updated_at: '2026-01-03T00:00:00Z', unread: false },
    { session_id: 'session-old-unread', updated_at: '2026-01-02T00:00:00Z', unread: true },
    { session_id: 'session-new-unread', updated_at: '2026-01-04T00:00:00Z', unread: true },
  ];

  it('renders unread badges and sorts unread sessions first (then recency)', () => {
    render(
      <SessionPicker
        agentName="test-agent"
        sessions={sessions}
        sessionsError={null}
        sessionsLoading={false}
        hasLoadedSessions={true}
        onRetry={vi.fn()}
        onSelect={vi.fn()}
        onNewChat={vi.fn()}
        onBack={vi.fn()}
      />
    );

    const items = screen.getAllByRole('button').filter(b => b.classList.contains('session-item'));
    expect(items).toHaveLength(4);
    // Order: session-new-unread, session-old-unread, session-new-read, session-old-read
    expect(items[0]).toHaveTextContent('session-new-unread');
    expect(items[1]).toHaveTextContent('session-old-unread');
    expect(items[2]).toHaveTextContent('session-new-read');
    expect(items[3]).toHaveTextContent('session-old-read');

    const badges = screen.getAllByTestId('session-unread-badge');
    expect(badges).toHaveLength(2);
    expect(badges[0]).toHaveTextContent('Unread');
    expect(badges[1]).toHaveTextContent('Unread');
  });

  it('toggles read/unread without triggering card selection', () => {
    const onSelect = vi.fn();
    const onToggleUnread = vi.fn();

    render(
      <SessionPicker
        agentName="test-agent"
        sessions={sessions}
        sessionsError={null}
        sessionsLoading={false}
        hasLoadedSessions={true}
        onRetry={vi.fn()}
        onSelect={onSelect}
        onNewChat={vi.fn()}
        onBack={vi.fn()}
        onToggleUnread={onToggleUnread}
      />
    );

    // Find the toggle button on the first session (which is unread)
    const markReadBtn = screen.getByRole('button', { name: 'Mark session session-new-unread as read' });
    expect(markReadBtn).toHaveTextContent('Mark read');

    fireEvent.click(markReadBtn);
    expect(onToggleUnread).toHaveBeenCalledWith('session-new-unread', true);
    expect(onSelect).not.toHaveBeenCalled();

    // Find the toggle button on the third session (which is read)
    const markUnreadBtn = screen.getByRole('button', { name: 'Mark session session-new-read as unread' });
    expect(markUnreadBtn).toHaveTextContent('Mark unread');

    fireEvent.click(markUnreadBtn);
    expect(onToggleUnread).toHaveBeenCalledWith('session-new-read', false);
    expect(onSelect).not.toHaveBeenCalled();
  });
});
