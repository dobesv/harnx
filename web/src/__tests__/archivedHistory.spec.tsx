import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';

import { ArchivedHistoryViewer } from '../ArchivedHistoryViewer';
import { ArchivedHistoryProvider } from '../ArchivedHistoryProvider';
import { CompactionMarker } from '../CompactionMarker';
import { CompactionMetadataContext } from '../CompactionMetadataContext';
import * as api from '../api';
import type * as aui from '@assistant-ui/react';
import type { SessionHistoryResult } from '../types';

vi.mock('@assistant-ui/react', async (importOriginal) => {
  const actual = await importOriginal<typeof aui>();
  return {
    ...actual,
    MessagePrimitive: {
      Root: ({ className, children, ...props }: any) => (
        <div className={className} {...props}>
          {children}
        </div>
      ),
      Content: () => null,
    },
  };
});

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof api>();
  return {
    ...actual,
    getSessionHistory: vi.fn(),
  };
});

describe('ArchivedHistoryViewer (D4 browse-back)', () => {
  const mockGetSessionHistory = vi.mocked(api.getSessionHistory);

  beforeEach(() => {
    vi.resetAllMocks();
  });

  // Fixture response shapes matching note dee8b997
  const window15Page1 = {
    history_snapshot: [
      {
        role: 'assistant',
        id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
        content: 'The session log stores tool calls and results separately. No messages were kept.',
      },
      {
        role: 'user',
        id: '3e3bc161-3752-5646-a160-c7da8062e7c0',
        content: 'Implement the shared transcript view and verify paging.',
      },
    ],
    compaction: {
      message_id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
      compaction_seq: 5,
      summary: 'The session log stores tool calls and results separately. No messages were kept.',
      from_seq: 1,
      to_seq: 4,
      kept_count: 0,
    },
    next_cursor: 'cursor-page-2',
    prev_cursor: null,
    earlier_compaction: 5,
  };

  const window15Page2 = {
    history_snapshot: [
      {
        role: 'assistant',
        id: 'retained-reply-id',
        content: 'The shared view is implemented. Paging tests passed.',
      },
    ],
    next_cursor: null,
    prev_cursor: 'cursor-page-1',
    earlier_compaction: 5,
  };

  const window5Page1 = {
    history_snapshot: [
      {
        role: 'user',
        id: 'f184b0f4-5008-5705-8979-31bf8cc66c05',
        content: 'Inspect the session log format.',
      },
      {
        role: 'assistant',
        id: 'ac5ec19d-b65e-5eca-92af-74349ef65e1f',
        content: 'Tool calls and results are separate log entries.',
      },
    ],
    next_cursor: null,
    prev_cursor: null,
    earlier_compaction: null,
  };

  it('loads and renders archived history for a compaction window', async () => {
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    expect(screen.getByText('Loading archived history…')).toBeInTheDocument();

    await waitFor(() => {
      expect(screen.getByText('History before compaction 15')).toBeInTheDocument();
    });

    expect(mockGetSessionHistory).toHaveBeenCalledWith(
      'coder',
      's-1',
      {
        before_compaction: 15,
      },
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );

    // Check message rendering (synthetic compaction marker in earlier window + user message)
    expect(screen.getByTestId('archived-compaction-marker')).toBeInTheDocument();
    expect(
      screen.getByText('Implement the shared transcript view and verify paging.'),
    ).toBeInTheDocument();
  });

  it('pages via next_cursor and prev_cursor', async () => {
    mockGetSessionHistory
      .mockResolvedValueOnce(window15Page1)
      .mockResolvedValueOnce(window15Page2)
      .mockResolvedValueOnce(window15Page1);

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Next page' })).toBeEnabled();
    });

    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();

    // Click Next
    fireEvent.click(screen.getByRole('button', { name: 'Next page' }));

    await waitFor(() => {
      expect(
        screen.getByText('The shared view is implemented. Paging tests passed.'),
      ).toBeInTheDocument();
    });

    expect(mockGetSessionHistory).toHaveBeenCalledWith(
      'coder',
      's-1',
      {
        cursor: 'cursor-page-2',
      },
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeEnabled();

    // Click Previous
    fireEvent.click(screen.getByRole('button', { name: 'Previous page' }));

    await waitFor(() => {
      expect(
        screen.getByText('Implement the shared transcript view and verify paging.'),
      ).toBeInTheDocument();
    });

    expect(mockGetSessionHistory).toHaveBeenCalledWith(
      'coder',
      's-1',
      {
        cursor: 'cursor-page-1',
      },
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
  });

  it('keeps focus inside the dialog after next/prev page loads rather than dropping to body', async () => {
    mockGetSessionHistory
      .mockResolvedValueOnce(window15Page1)
      .mockResolvedValueOnce(window15Page2)
      .mockResolvedValueOnce(window15Page1);

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    const dialog = await screen.findByRole('dialog');
    const nextBtn = await screen.findByRole('button', { name: 'Next page' });

    // Focus next page button and click it
    nextBtn.focus();
    expect(document.activeElement).toBe(nextBtn);
    fireEvent.click(nextBtn);

    // Wait for page 2 to load
    await waitFor(() => {
      expect(
        screen.getByText('The shared view is implemented. Paging tests passed.'),
      ).toBeInTheDocument();
    });

    // Active element must not be document.body, and must be the content container with class archived-history-content
    expect(document.activeElement).not.toBe(document.body);
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(document.activeElement).toHaveClass('archived-history-content');

    // Now click Previous page
    const prevBtn = screen.getByRole('button', { name: 'Previous page' });
    prevBtn.focus();
    expect(document.activeElement).toBe(prevBtn);
    fireEvent.click(prevBtn);

    // Wait for page 1 to load
    await waitFor(() => {
      expect(
        screen.getByText('Implement the shared transcript view and verify paging.'),
      ).toBeInTheDocument();
    });

    // Active element must remain inside the dialog on the content container
    expect(document.activeElement).not.toBe(document.body);
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(document.activeElement).toHaveClass('archived-history-content');
  });

  it('recovers focus to the content container when paging fails with an error', async () => {
    mockGetSessionHistory
      .mockResolvedValueOnce(window15Page1)
      .mockRejectedValueOnce(new Error('Network error on page 2'));

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    const dialog = await screen.findByRole('dialog');
    const nextBtn = await screen.findByRole('button', { name: 'Next page' });

    nextBtn.focus();
    expect(document.activeElement).toBe(nextBtn);
    fireEvent.click(nextBtn);

    await waitFor(() => {
      expect(screen.getByText('Network error on page 2')).toBeInTheDocument();
    });

    // Focus must not drop to body; it must recover to archived-history-content
    expect(document.activeElement).not.toBe(document.body);
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(document.activeElement).toHaveClass('archived-history-content');
  });

  it('walks back through 2 compactions to the first message (nested compaction walk) and goes back', async () => {
    mockGetSessionHistory
      .mockResolvedValueOnce(window15Page1) // before_compaction: 15
      .mockResolvedValueOnce(window5Page1); // before_compaction: 5

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    // Initial window before 15 loaded
    await waitFor(() => {
      expect(screen.getByTestId('archived-earlier-history-btn')).toBeInTheDocument();
    });

    const backBtn = screen.getByRole('button', { name: 'Back to more recent archived history' });
    expect(backBtn).toBeDisabled();

    // Click "Show earlier history" on earlier marker inside the viewer
    fireEvent.click(screen.getByTestId('archived-earlier-history-btn'));

    // Loads window before 5
    await waitFor(
      () => {
        expect(screen.getByText('History before compaction 5')).toBeInTheDocument();
      },
      { timeout: 2000 },
    );

    expect(mockGetSessionHistory).toHaveBeenCalledWith(
      'coder',
      's-1',
      {
        before_compaction: 5,
      },
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );

    // Contains first prompt and first reply from fixture
    expect(screen.getByText('Inspect the session log format.')).toBeInTheDocument();
    expect(
      screen.getByText('Tool calls and results are separate log entries.'),
    ).toBeInTheDocument();

    // Now back button is enabled
    expect(backBtn).toBeEnabled();

    // Click Back to return to window 15
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);
    fireEvent.click(backBtn);

    await waitFor(() => {
      expect(screen.getByText('History before compaction 15')).toBeInTheDocument();
    });
  });

  it('renders error state when session/history fails', async () => {
    mockGetSessionHistory.mockRejectedValueOnce(new Error('RPC Error: invalid sequence'));

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 99,
        }}
        onClose={() => {}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByRole('alert')).toBeInTheDocument();
      expect(screen.getByText('RPC Error: invalid sequence')).toBeInTheDocument();
    });
  });

  it('closes on Escape key and on close button click', async () => {
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);
    const onClose = vi.fn();

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={onClose}
      />,
    );

    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Close archived history viewer' })).toBeInTheDocument();
    });

    // Press Escape
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);

    // Click close button
    fireEvent.click(screen.getByRole('button', { name: 'Close archived history viewer' }));
    expect(onClose).toHaveBeenCalledTimes(2);
  });
  it('traps focus within the dialog on Tab and Shift+Tab and restores focus on close', async () => {
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);
    const triggerBtn = document.createElement('button');
    triggerBtn.textContent = 'Trigger';
    document.body.appendChild(triggerBtn);
    triggerBtn.focus();
    expect(document.activeElement).toBe(triggerBtn);

    const { unmount } = render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    const closeBtn = await screen.findByRole('button', {
      name: 'Close archived history viewer',
    });
    // Initial focus lands on close button
    expect(document.activeElement).toBe(closeBtn);

    const nextPageBtn = screen.getByRole('button', { name: 'Next page' });

    // Focus last focusable element in dialog (next page button)
    nextPageBtn.focus();
    expect(document.activeElement).toBe(nextPageBtn);

    // Tab from last focusable wraps to first focusable (close button, since back is disabled)
    fireEvent.keyDown(window, { key: 'Tab' });
    expect(document.activeElement).toBe(closeBtn);

    // Shift+Tab from first focusable wraps to last focusable (nextPageBtn)
    fireEvent.keyDown(window, { key: 'Tab', shiftKey: true });
    expect(document.activeElement).toBe(nextPageBtn);

    // Unmount restores focus to triggerBtn
    unmount();
    expect(document.activeElement).toBe(triggerBtn);
    document.body.removeChild(triggerBtn);
  });

  it('marks .app-container inert and locks body scroll while open', async () => {
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);

    const appContainer = document.createElement('div');
    appContainer.className = 'app-container';
    document.body.appendChild(appContainer);

    expect(appContainer.getAttribute('inert')).toBeNull();
    expect(document.body.style.overflow).toBe('');

    const { unmount } = render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    await screen.findByRole('button', { name: 'Close archived history viewer' });

    expect(appContainer.getAttribute('inert')).toBe('');
    expect(appContainer.getAttribute('aria-hidden')).toBe('true');
    expect(document.body.style.overflow).toBe('hidden');

    unmount();

    expect(appContainer.getAttribute('inert')).toBeNull();
    expect(appContainer.getAttribute('aria-hidden')).toBeNull();
    expect(document.body.style.overflow).toBe('');
    document.body.removeChild(appContainer);
  });

  it('restores focus to opener only after inert is removed from app container', async () => {
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);

    const appContainer = document.createElement('div');
    appContainer.className = 'app-container';
    const openerBtn = document.createElement('button');
    openerBtn.textContent = 'Open history';
    appContainer.appendChild(openerBtn);
    document.body.appendChild(appContainer);

    openerBtn.focus();
    expect(document.activeElement).toBe(openerBtn);

    let inertStateWhenOpenerFocused: string | null = 'not-called';
    const originalFocus = openerBtn.focus.bind(openerBtn);
    vi.spyOn(openerBtn, 'focus').mockImplementation(() => {
      inertStateWhenOpenerFocused = appContainer.getAttribute('inert');
      originalFocus();
    });

    const { unmount } = render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    await screen.findByRole('button', { name: 'Close archived history viewer' });
    expect(appContainer.getAttribute('inert')).toBe('');

    unmount();

    expect(inertStateWhenOpenerFocused).toBeNull();
    expect(document.activeElement).toBe(openerBtn);

    document.body.removeChild(appContainer);
  });

  it('does not attempt to focus opener if it was removed from DOM', async () => {
    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);

    const appContainer = document.createElement('div');
    appContainer.className = 'app-container';
    const openerBtn = document.createElement('button');
    openerBtn.textContent = 'Ephemeral opener';
    appContainer.appendChild(openerBtn);
    document.body.appendChild(appContainer);

    openerBtn.focus();
    const focusSpy = vi.spyOn(openerBtn, 'focus');

    const { unmount } = render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    await screen.findByRole('button', { name: 'Close archived history viewer' });

    // Remove opener from DOM before viewer unmounts
    appContainer.removeChild(openerBtn);
    expect(openerBtn.isConnected).toBe(false);
    focusSpy.mockClear();

    expect(() => unmount()).not.toThrow();
    expect(focusSpy).not.toHaveBeenCalled();

    document.body.removeChild(appContainer);
  });

  it('aborts stale in-flight requests and avoids race conditions', async () => {
    let resolveFirst: ((val: SessionHistoryResult) => void) | null = null;
    const firstPromise = new Promise<SessionHistoryResult>((resolve) => {
      resolveFirst = resolve;
    });

    mockGetSessionHistory.mockReturnValueOnce(firstPromise);

    const { rerender } = render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    // Fast target change / rerender before first resolves
    mockGetSessionHistory.mockResolvedValueOnce(window5Page1);

    rerender(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 5,
        }}
        onClose={() => {}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText('History before compaction 5')).toBeInTheDocument();
    });

    // Now late first promise resolves
    resolveFirst!(window15Page1);

    // Stale resolution should NOT overwrite the current target
    await new Promise((r) => setTimeout(r, 50));
    expect(screen.getByText('History before compaction 5')).toBeInTheDocument();
    expect(screen.queryByText('History before compaction 15')).not.toBeInTheDocument();
  });

  it('allows retrying after a load error', async () => {
    mockGetSessionHistory.mockRejectedValueOnce(new Error('Network offline'));

    render(
      <ArchivedHistoryViewer
        initialTarget={{
          agent: 'coder',
          session: 's-1',
          beforeCompaction: 15,
        }}
        onClose={() => {}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText('Network offline')).toBeInTheDocument();
    });

    const retryBtn = screen.getByRole('button', { name: 'Retry' });
    expect(retryBtn).toBeInTheDocument();

    mockGetSessionHistory.mockResolvedValueOnce(window15Page1);
    fireEvent.click(retryBtn);

    await waitFor(() => {
      expect(screen.getByText('History before compaction 15')).toBeInTheDocument();
    });
  });
});

describe('CompactionMarker action integration with ArchivedHistoryProvider', () => {
  const mockGetSessionHistory = vi.mocked(api.getSessionHistory);

  beforeEach(() => {
    vi.resetAllMocks();
  });

  it('clicking "Show earlier history" on the compaction marker opens the viewer', async () => {
    mockGetSessionHistory.mockResolvedValueOnce({
      history_snapshot: [],
      next_cursor: null,
      prev_cursor: null,
      earlier_compaction: null,
    });

    const metadata = {
      message_id: 'bbdf1d00-eab3-5347-91b7-d9d60d285b7f',
      compaction_seq: 15,
      summary: 'Earlier log inspection is archived.',
      from_seq: 6,
      to_seq: 14,
      earlier_compaction: 5,
    };

    render(
      <ArchivedHistoryProvider>
        <CompactionMetadataContext.Provider
          value={{
            metadata,
            agent: 'test-agent',
            session: 'test-session',
          }}
        >
          <CompactionMarker metadata={metadata} />
        </CompactionMetadataContext.Provider>
      </ArchivedHistoryProvider>,
    );

    const btn = screen.getByTestId('show-earlier-history-btn');
    expect(btn).toBeInTheDocument();
    expect(screen.queryByTestId('archived-history-viewer')).not.toBeInTheDocument();

    fireEvent.click(btn);

    await waitFor(() => {
      expect(screen.getByTestId('archived-history-viewer')).toBeInTheDocument();
    });

    expect(mockGetSessionHistory).toHaveBeenCalledWith(
      'test-agent',
      'test-session',
      {
        before_compaction: 15,
      },
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
  });
});
