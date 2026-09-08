class MockEventSource {
  close() {}
  addEventListener() {}
}
(globalThis as any).EventSource = MockEventSource;

import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { BatchInterruptUI, MyComposer } from '../App';
import { PendingContext } from '../PendingContext';
import * as agUi from '@assistant-ui/react-ag-ui';
import * as aui from '@assistant-ui/react';
import { vi } from 'vitest';
import { ChatProvider } from '../ChatProvider';

vi.mock('@assistant-ui/react-ag-ui', async (importOriginal) => {
  const actual = await importOriginal<typeof agUi>();
  return { ...actual, useAgUiInterrupts: vi.fn(), useAgUiSubmitInterruptResponses: vi.fn() };
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

describe('MyComposer - Queued Messages', () => {
  afterEach(() => {
    vi.clearAllMocks();
  });

  it('queues a message while running, and edit restores it', async () => {
    const setErrorText = vi.fn();
    vi.mocked(aui.useAuiState).mockReturnValue(true); // isRunning = true
    
    let composerText = 'my pending text';
    const composerRuntime = {
      getState: () => ({ text: composerText, attachments: [] }),
      setText: vi.fn(),
      clearAttachments: vi.fn(),
      addAttachment: vi.fn(),
      send: vi.fn(),
      subscribe: vi.fn(() => () => {}),
    };
    vi.mocked(aui.useAui).mockReturnValue({ composer: composerRuntime } as any);

    render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
          <MyComposer agentName="foo" sessionId="bar" switchAgentHref="" switchSessionHref="" onSwitchAgent={() => {}} onSwitchSession={() => {}} />
        </PendingContext.Provider>
      </ChatProvider>
    );

    // Instead of using form submission, let's just trigger the onSubmit manually or find the form
    // The Root is an actual ComposerPrimitive.Root, which we didn't mock now.
    // It should render properly if ChatProvider is there.
    
    const form = document.querySelector('form');
    expect(form).not.toBeNull();
    fireEvent.submit(form!);

    expect(screen.getByTestId('queued-message')).toBeInTheDocument();
    expect(screen.getByText('my pending text')).toBeInTheDocument();
    expect(composerRuntime.setText).toHaveBeenCalledWith('');
    expect(composerRuntime.clearAttachments).toHaveBeenCalled();
    expect(composerRuntime.send).not.toHaveBeenCalled();

    fireEvent.click(screen.getByText('Edit'));

    expect(composerRuntime.setText).toHaveBeenCalledWith('my pending text');
    expect(screen.queryByTestId('queued-message')).not.toBeInTheDocument();
  });

  it('cancel clears the queued message', async () => {
    const setErrorText = vi.fn();
    vi.mocked(aui.useAuiState).mockReturnValue(true); // isRunning = true
    
    let composerText = 'another pending';
    const composerRuntime = {
      getState: () => ({ text: composerText, attachments: [] }),
      setText: vi.fn(),
      clearAttachments: vi.fn(),
      addAttachment: vi.fn(),
      send: vi.fn(),
      subscribe: vi.fn(() => () => {}),
    };
    vi.mocked(aui.useAui).mockReturnValue({ composer: composerRuntime } as any);

    render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
          <MyComposer agentName="foo" sessionId="bar" switchAgentHref="" switchSessionHref="" onSwitchAgent={() => {}} onSwitchSession={() => {}} />
        </PendingContext.Provider>
      </ChatProvider>
    );

    const form = document.querySelector('form');
    fireEvent.submit(form!);

    expect(screen.getByTestId('queued-message')).toBeInTheDocument();

    fireEvent.click(screen.getByText('Cancel'));

    expect(screen.queryByTestId('queued-message')).not.toBeInTheDocument();
  });

  it('auto-flushes queued message when run finishes (isRunning goes false)', async () => {
    const setErrorText = vi.fn();
    // Start with isRunning = true
    let isRunning = true;
    vi.mocked(aui.useAuiState).mockImplementation(() => isRunning);
    
    let composerText = 'auto flush text';
    const composerRuntime = {
      getState: () => ({ text: composerText, attachments: [] }),
      setText: vi.fn(),
      clearAttachments: vi.fn(),
      addAttachment: vi.fn(),
      send: vi.fn(),
      subscribe: vi.fn(() => () => {}),
    };
    vi.mocked(aui.useAui).mockReturnValue({ composer: composerRuntime } as any);

    const { rerender } = render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
          <MyComposer agentName="foo" sessionId="bar" switchAgentHref="" switchSessionHref="" onSwitchAgent={() => {}} onSwitchSession={() => {}} />
        </PendingContext.Provider>
      </ChatProvider>
    );

    const form = document.querySelector('form');
    fireEvent.submit(form!);

    expect(screen.getByTestId('queued-message')).toBeInTheDocument();
    
    // Simulate run finishing
    isRunning = false;
    rerender(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
          <MyComposer agentName="foo" sessionId="bar" switchAgentHref="" switchSessionHref="" onSwitchAgent={() => {}} onSwitchSession={() => {}} />
        </PendingContext.Provider>
      </ChatProvider>
    );

    expect(composerRuntime.setText).toHaveBeenCalledWith('auto flush text');
    expect(composerRuntime.send).toHaveBeenCalled();
    expect(screen.queryByTestId('queued-message')).not.toBeInTheDocument();
  });

  it('queues attachments, edit restores them, and auto-flush re-attaches them', async () => {
    const setErrorText = vi.fn();
    let isRunning = true;
    vi.mocked(aui.useAuiState).mockImplementation(() => isRunning);
    
    const mockFile1 = new File([''], 'file1.png');
    const mockFile2 = new File([''], 'file2.png');
    const mockAttachments = [
      { status: { type: 'complete' }, file: mockFile1 },
      { status: { type: 'complete' }, file: mockFile2 }
    ];
    
    let composerText = 'text with attachments';
    const composerRuntime = {
      getState: () => ({ text: composerText, attachments: mockAttachments }),
      setText: vi.fn(),
      clearAttachments: vi.fn(),
      addAttachment: vi.fn(),
      send: vi.fn(),
      subscribe: vi.fn(() => () => {}),
    };
    vi.mocked(aui.useAui).mockReturnValue({ composer: composerRuntime } as any);

    const { rerender } = render(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
          <MyComposer agentName="foo" sessionId="bar" switchAgentHref="" switchSessionHref="" onSwitchAgent={() => {}} onSwitchSession={() => {}} />
        </PendingContext.Provider>
      </ChatProvider>
    );

    const form = document.querySelector('form');
    fireEvent.submit(form!);

    // Queued banner should show attachment count
    expect(screen.getByTestId('queued-message')).toBeInTheDocument();
    expect(screen.getByText('[2 attachments]')).toBeInTheDocument();

    // Edit restores them
    fireEvent.click(screen.getByText('Edit'));
    expect(composerRuntime.addAttachment).toHaveBeenCalledTimes(2);
    expect(composerRuntime.addAttachment).toHaveBeenCalledWith(mockFile1);
    expect(composerRuntime.addAttachment).toHaveBeenCalledWith(mockFile2);
    
    // Re-queue the message to test flush
    fireEvent.submit(form!);
    expect(screen.getByTestId('queued-message')).toBeInTheDocument();
    
    // Clear mock calls to distinguish from Edit
    vi.mocked(composerRuntime.addAttachment).mockClear();

    // Flush on idle
    isRunning = false;
    rerender(
      <ChatProvider agentName="foo" sessionId="bar" isFreshSession={false} onOpenSubAgent={vi.fn()}>
        <PendingContext.Provider value={{ setErrorText, setStatusText: vi.fn(), statusText: null, errorText: null }}>
          <MyComposer agentName="foo" sessionId="bar" switchAgentHref="" switchSessionHref="" onSwitchAgent={() => {}} onSwitchSession={() => {}} />
        </PendingContext.Provider>
      </ChatProvider>
    );

    expect(composerRuntime.addAttachment).toHaveBeenCalledTimes(2);
    expect(composerRuntime.addAttachment).toHaveBeenCalledWith(mockFile1);
    expect(composerRuntime.send).toHaveBeenCalled();
  });
});
