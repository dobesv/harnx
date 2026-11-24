import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';
import * as aui from '@assistant-ui/react';
import { HistoryReasoningSupportContext } from '../historyReasoningSupport';
import { MessageTextPart } from '../components/MessageTextPart';

let currentText = '';
let currentRole = 'assistant';

vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return {
    ...actual,
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

describe('MessageTextPart legacy inline think fallback', () => {
  beforeEach(() => {
    currentText = '';
    currentRole = 'assistant';
  });

  it('segments inline <think> tags for assistant messages', () => {
    currentRole = 'assistant';
    currentText = '<think>\nLegacy thought\n</think>\n\nLegacy answer';

    const { container } = render(<MessageTextPart />);

    const block = container.querySelector('.aui-thinking-block');
    expect(block).toBeInTheDocument();
    expect(block).not.toHaveAttribute('open');
    expect(screen.getByText('Legacy thought')).toBeInTheDocument();
    expect(screen.getByTestId('segmented-text-part')).toHaveAttribute('data-text', '\n\nLegacy answer');
  });

  it('does not segment inline <think> tags for user messages', () => {
    currentRole = 'user';
    currentText = 'Please explain <think>example</think> tags';

    const { container } = render(<MessageTextPart />);

    expect(container.querySelector('.aui-thinking-block')).toBeNull();
    expect(screen.getByTestId('markdown-primitive')).toHaveTextContent(
      'Please explain <think>example</think> tags',
    );
  });

  it('renders literal <think> tags verbatim for assistant messages when history_reasoning is supported', () => {
    currentRole = 'assistant';
    currentText = 'Literal <think>example</think> in answer';

    const { container } = render(
      <HistoryReasoningSupportContext.Provider value={true}>
        <MessageTextPart />
      </HistoryReasoningSupportContext.Provider>,
    );

    expect(container.querySelector('.aui-thinking-block')).toBeNull();
    expect(screen.getByTestId('markdown-primitive')).toHaveTextContent(
      'Literal <think>example</think> in answer',
    );
  });
});
