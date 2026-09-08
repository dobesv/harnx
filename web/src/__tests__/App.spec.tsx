class MockEventSource {
  close() {}
  addEventListener() {}
}
(globalThis as any).EventSource = MockEventSource;

import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { BatchInterruptUI, MyComposer } from '../App';
import { sendPrompt, uploadAttachment } from '../api';
import { PendingContext } from '../PendingContext';
import * as agUi from '@assistant-ui/react-ag-ui';
import * as aui from '@assistant-ui/react';
import { vi } from 'vitest';
import { ChatProvider } from '../ChatProvider';

vi.mock('@assistant-ui/react-ag-ui', async (importOriginal) => {
  const actual = await importOriginal<typeof agUi>();
  return { ...actual, useAgUiInterrupts: vi.fn(), useAgUiSubmitInterruptResponses: vi.fn() };
});

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return { ...actual, sendPrompt: vi.fn(), uploadAttachment: vi.fn() };
});
vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return { 
    ...actual, 
    useAuiState: vi.fn(), 
    useAui: vi.fn(),
  };
});

describe('BatchInterruptUI', () => {
  afterEach(() => {
    vi.clearAllMocks();
  });

  it('submits exact payload for approve and deny', async () => {
    const setErrorText = vi.fn();
    const submitResponses = vi.fn().mockResolvedValue(undefined);
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-1', toolCallId: 'tool-1', reason: '' } as any]);
    vi.mocked(agUi.useAgUiSubmitInterruptResponses).mockReturnValue(submitResponses);

    render(
      <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
        <BatchInterruptUI />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByLabelText(/Approve/));
    fireEvent.click(screen.getByText(/Submit Decisions/));

    await waitFor(() => {
      expect(submitResponses).toHaveBeenCalledWith([{ interruptId: 'int-1', status: 'resolved', payload: { approved: true } }]);
    });

    fireEvent.click(screen.getByLabelText(/Deny/));
    fireEvent.click(screen.getByText(/Submit Decisions/));

    await waitFor(() => {
      expect(submitResponses).toHaveBeenCalledWith([{ interruptId: 'int-1', status: 'cancelled', payload: { approved: false } }]);
    });
  });

  it('surfaces an error on rejection', async () => {
    const setErrorText = vi.fn();
    const submitResponses = vi.fn().mockRejectedValue(new Error('Network error'));
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([{ id: 'int-2', toolCallId: 'tool-2', reason: '' } as any]);
    vi.mocked(agUi.useAgUiSubmitInterruptResponses).mockReturnValue(submitResponses);

    render(
      <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
        <BatchInterruptUI />
      </PendingContext.Provider>
    );

    fireEvent.click(screen.getByLabelText(/Approve/));
    fireEvent.click(screen.getByText(/Submit Decisions/));

    await waitFor(() => {
      expect(setErrorText).toHaveBeenCalledWith('Network error');
    });
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
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
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
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
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
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
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
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
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
