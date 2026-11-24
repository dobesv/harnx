import { describe, it, expect, vi, beforeEach } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';
import type { ReactNode } from 'react';
import type * as aui from '@assistant-ui/react';

let capturedAgent: any;
let currentText = '<think>\nInner thought\n</think>\n\nAnswer text';
let currentRole = 'assistant';

vi.mock('@assistant-ui/react-ag-ui', () => ({
  useAgUiRuntime: ({ agent }: { agent: unknown }) => {
    capturedAgent = agent;
    return {};
  },
}));

vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return {
    ...actual,
    AssistantRuntimeProvider: ({ children }: { children: ReactNode }) => <>{children}</>,
    useAuiState: (selector: (state: any) => unknown) =>
      selector({ message: { role: currentRole } }),
    useMessagePartText: () => ({ type: 'text', text: currentText, status: { type: 'complete' } }),
    useSmooth: (part: any) => ({ text: part.text, status: part.status }),
    TextMessagePartProvider: ({ text, children }: any) => (
      <div data-testid="segmented-text-part" data-text={text}>
        {children}
      </div>
    ),
  };
});

vi.mock('@assistant-ui/react-markdown', () => ({
  MarkdownTextPrimitive: () => <span data-testid="markdown-primitive">{currentText}</span>,
}));

vi.mock('../RuntimeSessionSubscriber', () => ({ RuntimeSessionSubscriber: () => null }));

import { ChatProvider } from '../ChatProvider';
import { MessageTextPart } from '../components/MessageTextPart';

async function startRun(agent: any): Promise<any> {
  const subscriber: any = {};
  const spy = vi
    .spyOn(Object.getPrototypeOf(Object.getPrototypeOf(agent)), 'runAgent')
    .mockImplementation((_params: any, sub: any) => {
      Object.assign(subscriber, sub);
      return Promise.resolve();
    });
  try {
    await agent.runAgent({});
  } finally {
    spy.mockRestore();
  }
  return subscriber;
}

describe('ChatProvider -> HistoryReasoningSupportContext -> MessageTextPart', () => {
  beforeEach(() => {
    capturedAgent = undefined;
    currentRole = 'assistant';
    currentText = '<think>\nInner thought\n</think>\n\nAnswer text';
  });

  it('switches MessageTextPart from legacy fallback to literal rendering on history_reasoning and resets on session change', async () => {
    const { container, rerender } = render(
      <ChatProvider
        agentName="agent"
        sessionId="session-1"
        isFreshSession={false}
        onOpenSubAgent={() => {}}
      >
        <MessageTextPart />
      </ChatProvider>,
    );

    // Initially false: inline <think> tags render as a ThinkingBlock fallback.
    expect(container.querySelector('.aui-thinking-block')).toBeInTheDocument();
    expect(screen.getByText('Inner thought')).toBeInTheDocument();

    // Arrival of history_reasoning flips support false -> true: literal tags render verbatim.
    const session1Agent = capturedAgent;
    const subscriber1 = await startRun(session1Agent);
    await act(async () => {
      await subscriber1.onEvent({
        event: { type: 'CUSTOM', name: 'history_reasoning', value: {} },
      });
    });

    expect(container.querySelector('.aui-thinking-block')).toBeNull();
    expect(screen.getByTestId('markdown-primitive')).toHaveTextContent(
      '<think> Inner thought </think> Answer text',
    );

    // Switching sessions resets support back to false and ignores late events from the old agent.
    rerender(
      <ChatProvider
        agentName="agent"
        sessionId="session-2"
        isFreshSession={false}
        onOpenSubAgent={() => {}}
      >
        <MessageTextPart />
      </ChatProvider>,
    );

    expect(container.querySelector('.aui-thinking-block')).toBeInTheDocument();
    expect(screen.getByText('Inner thought')).toBeInTheDocument();

    await act(async () => {
      await subscriber1.onEvent({
        event: { type: 'CUSTOM', name: 'history_reasoning', value: { a1: ['stale'] } },
      });
    });
    expect(container.querySelector('.aui-thinking-block')).toBeInTheDocument();

    // New session's own history_reasoning flips support true again.
    const subscriber2 = await startRun(capturedAgent);
    await act(async () => {
      await subscriber2.onEvent({
        event: { type: 'CUSTOM', name: 'history_reasoning', value: { a2: ['fresh'] } },
      });
    });
    expect(container.querySelector('.aui-thinking-block')).toBeNull();
  });
});
