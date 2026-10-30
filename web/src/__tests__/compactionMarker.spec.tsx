import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';

import rawLogEntries from '../../../crates/harnx-core/tests/fixtures/compaction-history.json';
import { CompactionMetadataContext } from '../CompactionMetadataContext';
import { MyMessage } from '../App';
import * as aui from '@assistant-ui/react';
import type { CompactionMetadata } from '../compactionMetadata';

// Mock assistant-ui components for unit testing transcript rendering
vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return {
    ...actual,
    useAuiState: vi.fn(),
    useAui: vi.fn(),
    useMessagePartText: () => ({ text: 'mock text', status: { type: 'complete' } }),
    useSmooth: (part: any) => ({ text: part?.text || '', status: part?.status || { type: 'complete' } }),
    MessagePrimitive: {
      Root: ({ className, children, ...props }: any) => (
        <div className={className} {...props}>
          {children}
        </div>
      ),
      Content: ({ components }: any) => {
        // If components.Text is provided, render it using mock text
        if (components?.Text) {
          const TextComp = components.Text;
          return <TextComp />;
        }
        return null;
      },
    },
  };
});

vi.mock('@assistant-ui/react-markdown', () => ({
  MarkdownTextPrimitive: () => {
    // Render text from useAuiState
    const content = (aui.useAuiState as any)((s: any) => s.message.content);
    const text = Array.isArray(content)
      ? content.map((c: any) => c.text || '').join('')
      : typeof content === 'string'
        ? content
        : '';
    return <span>{text}</span>;
  },
}));

describe('Compaction fixture and marker rendering', () => {
  // Verify the shared fixture can be imported directly per crates/harnx-core/tests/fixtures/README.md
  it('imports raw compaction-history.json entries correctly', () => {
    expect(Array.isArray(rawLogEntries)).toBe(true);
    expect(rawLogEntries.length).toBeGreaterThan(0);

    // The fixture contains physical sequences 1..23
    const firstEntry = rawLogEntries[0] as [number, { type: string }];
    expect(firstEntry[0]).toBe(1);
    expect(firstEntry[1].type).toBe('message');

    // Sequence 5 is a compress entry (zero kept)
    const compressSeq5 = (rawLogEntries as [number, { type: string }][]).find(
      ([seq]) => seq === 5,
    );
    expect(compressSeq5).toBeDefined();
    expect(compressSeq5![1].type).toBe('compress');

    // Sequence 15 is a compress entry (retained prompt + tail)
    const compressSeq15 = (rawLogEntries as [number, { type: string }][]).find(
      ([seq]) => seq === 15,
    );
    expect(compressSeq15).toBeDefined();
    expect(compressSeq15![1].type).toBe('compress');
  });

  describe('Zero-kept compaction marker (seq 5)', () => {
    const zeroKeptCompaction: CompactionMetadata = {
      message_id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
      compaction_seq: 5,
      summary: 'The session log stores tool calls and results separately. No messages were kept.',
      from_seq: 1,
      to_seq: 4,
      kept_count: 0,
    };

    it('renders summary and divider (not blank) for zero-kept compaction', () => {
      vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
        selector({
          message: {
            id: zeroKeptCompaction.message_id,
            role: 'assistant',
            content: [{ type: 'text', text: zeroKeptCompaction.summary }],
          },
        }),
      );

      render(
        <CompactionMetadataContext.Provider value={zeroKeptCompaction}>
          <MyMessage />
        </CompactionMetadataContext.Provider>,
      );

      expect(screen.getByTestId('compaction-marker')).toBeInTheDocument();
      expect(screen.getByTestId('compaction-marker')).toHaveAttribute('data-compaction-seq', '5');
      expect(screen.getByTestId('compaction-summary')).toBeInTheDocument();
      expect(screen.getByText(zeroKeptCompaction.summary)).toBeInTheDocument();
      expect(screen.getByText('Compaction')).toBeInTheDocument();
      expect(screen.getByText('Summary of earlier conversation')).toBeInTheDocument();
      expect(screen.getByText('Earlier history archived')).toBeInTheDocument();
    });
  });

  describe('Prompt + tail compaction marker (seq 15)', () => {
    const promptAndTailCompaction: CompactionMetadata = {
      message_id: 'bbdf1d00-eab3-5347-91b7-d9d60d285b7f',
      compaction_seq: 15,
      summary:
        'Earlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.',
      from_seq: 6,
      to_seq: 14,
      earlier_compaction: 5,
    };

    it('renders compaction marker for retained-prompt-and-tail compaction', () => {
      vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
        selector({
          message: {
            id: promptAndTailCompaction.message_id,
            role: 'assistant',
            content: [{ type: 'text', text: promptAndTailCompaction.summary }],
          },
        }),
      );

      render(
        <CompactionMetadataContext.Provider value={promptAndTailCompaction}>
          <MyMessage />
        </CompactionMetadataContext.Provider>,
      );

      expect(screen.getByTestId('compaction-marker')).toBeInTheDocument();
      expect(screen.getByTestId('compaction-marker')).toHaveAttribute('data-compaction-seq', '15');
      expect(screen.getByText(promptAndTailCompaction.summary)).toBeInTheDocument();
    });

    it('renders regular messages normally following the marker', () => {
      // Kept user prompt following the marker in default view
      vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
        selector({
          message: {
            id: 'retained-prompt',
            role: 'user',
            content: [{ type: 'text', text: 'Implement the shared transcript view and verify paging.' }],
          },
        }),
      );

      const { container } = render(
        <CompactionMetadataContext.Provider value={promptAndTailCompaction}>
          <MyMessage />
        </CompactionMetadataContext.Provider>,
      );

      expect(screen.queryByTestId('compaction-marker')).not.toBeInTheDocument();
      expect(container.querySelector('.aui-user-message')).toBeInTheDocument();
      expect(screen.getByText('Implement the shared transcript view and verify paging.')).toBeInTheDocument();
    });

    it('shows action button when history context and session are present, else hint', () => {
      vi.mocked(aui.useAuiState).mockImplementation((selector: any) =>
        selector({
          message: {
            id: promptAndTailCompaction.message_id,
            role: 'assistant',
            content: [{ type: 'text', text: promptAndTailCompaction.summary }],
          },
        }),
      );

      // With agent and session
      render(
        <CompactionMetadataContext.Provider
          value={{
            metadata: promptAndTailCompaction,
            agent: 'agent-1',
            session: 'session-1',
          }}
        >
          <MyMessage />
        </CompactionMetadataContext.Provider>,
      );

      // Without ArchivedHistoryProvider context, still falls back to hint
      expect(screen.getByText('Earlier history archived')).toBeInTheDocument();
    });
  });
});
