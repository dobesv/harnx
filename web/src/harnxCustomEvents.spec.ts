import { afterEach, describe, expect, it, vi } from 'vitest';
import { handleHarnxCustomEvent, NAVIGATION_CONTROL_EVENTS, type HarnxCustomEventCallbacks } from './harnxCustomEvents';

const callbacks = (isForeground: boolean): HarnxCustomEventCallbacks => ({
  onStatus: () => {},
  onRunFailed: () => {},
  onUsage: () => {},
  onToolSummary: () => {},
  isForeground,
});

describe('harnx custom session title events', () => {
  afterEach(() => {
    document.title = '';
  });

  it('ignores title events from child observers', () => {
    document.title = 'existing title';

    handleHarnxCustomEvent(
      'session_title_updated',
      { title: 'child title' },
      callbacks(false),
    );

    expect(document.title).toBe('existing title');
  });

  it('updates title for the foreground session', () => {
    handleHarnxCustomEvent(
      'session_title_updated',
      { title: 'foreground title' },
      callbacks(true),
    );

    expect(document.title).toBe('harnx — foreground title');
  });
});

describe('harnx message_attachments event', () => {
  it('calls onMessageAttachments with parsed data for valid payload', () => {
    const onMessageAttachments = vi.fn();
    handleHarnxCustomEvent(
      'message_attachments',
      {
        messageId: 'msg-123',
        attachments: [
          { partIndex: 1, cid: 'cid:sha256abc', kind: 'image' },
          { partIndex: 2, cid: 'cid:sha256def', kind: 'image' },
        ],
      },
      { ...callbacks(true), onMessageAttachments },
    );

    expect(onMessageAttachments).toHaveBeenCalledWith('msg-123', [
      { partIndex: 1, cid: 'cid:sha256abc', kind: 'image' },
      { partIndex: 2, cid: 'cid:sha256def', kind: 'image' },
    ]);
  });

  it('ignores malformed events (missing messageId, non-array, bad attachment entry)', () => {
    const onMessageAttachments = vi.fn();
    const cbs = { ...callbacks(true), onMessageAttachments };

    // Missing messageId
    handleHarnxCustomEvent('message_attachments', { attachments: [] }, cbs);
    // Blank messageId
    handleHarnxCustomEvent('message_attachments', { messageId: '   ', attachments: [] }, cbs);
    // Non-string messageId
    handleHarnxCustomEvent('message_attachments', { messageId: 123, attachments: [] }, cbs);
    // Missing attachments
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1' }, cbs);
    // Non-array attachments
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: 'invalid' }, cbs);
    // Bad attachment entry: not an object
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: [null] }, cbs);
    // Bad attachment entry: missing cid
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: [{ partIndex: 0, kind: 'image' }] }, cbs);
    // Bad attachment entry: empty cid
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: [{ partIndex: 0, cid: '', kind: 'image' }] }, cbs);
    // Bad attachment entry: non-number partIndex
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: [{ partIndex: '0', cid: 'cid:abc', kind: 'image' }] }, cbs);
    // Bad attachment entry: negative partIndex
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: [{ partIndex: -1, cid: 'cid:abc', kind: 'image' }] }, cbs);
    // Bad attachment entry: non-image kind
    handleHarnxCustomEvent('message_attachments', { messageId: 'msg-1', attachments: [{ partIndex: 0, cid: 'cid:abc', kind: 'file' }] }, cbs);

    expect(onMessageAttachments).not.toHaveBeenCalled();
  });

  it('is included in NAVIGATION_CONTROL_EVENTS to prevent assistant-ui forwarding', () => {
    expect(NAVIGATION_CONTROL_EVENTS).toContain('message_attachments');
  });
});

describe('harnx compaction events', () => {
  it('calls onCompactingStarted with compaction_id', () => {
    const onCompactingStarted = vi.fn();
    handleHarnxCustomEvent(
      'session_compacting_started',
      { compaction_id: 'compact-123' },
      { ...callbacks(true), onCompactingStarted },
    );

    expect(onCompactingStarted).toHaveBeenCalledWith('compact-123');
  });

  it('calls onCompactingStarted without compaction_id (automatic compaction)', () => {
    const onCompactingStarted = vi.fn();
    handleHarnxCustomEvent(
      'session_compacting_started',
      {},
      { ...callbacks(true), onCompactingStarted },
    );

    expect(onCompactingStarted).toHaveBeenCalledWith(undefined);
  });

  it('calls onCompactingCompleted with compacted outcome', () => {
    const onCompactingCompleted = vi.fn();
    handleHarnxCustomEvent(
      'session_compacting_completed',
      { outcome: { status: 'compacted' }, compaction_id: 'compact-123' },
      { ...callbacks(true), onCompactingCompleted },
    );

    expect(onCompactingCompleted).toHaveBeenCalledWith({ status: 'compacted' }, 'compact-123');
  });

  it('calls onCompactingCompleted with unchanged outcome and detail', () => {
    const onCompactingCompleted = vi.fn();
    handleHarnxCustomEvent(
      'session_compacting_completed',
      { outcome: { status: 'unchanged', detail: 'Nothing to compact' } },
      { ...callbacks(true), onCompactingCompleted },
    );

    expect(onCompactingCompleted).toHaveBeenCalledWith({ status: 'unchanged', detail: 'Nothing to compact' }, undefined);
  });

  it('ignores completed event with invalid outcome', () => {
    const onCompactingCompleted = vi.fn();
    const cbs = { ...callbacks(true), onCompactingCompleted };

    // Missing outcome
    handleHarnxCustomEvent('session_compacting_completed', {}, cbs);
    // Invalid status
    handleHarnxCustomEvent('session_compacting_completed', { outcome: { status: 'invalid' } }, cbs);

    expect(onCompactingCompleted).not.toHaveBeenCalled();
  });

  it('calls onCompactingFailed with error and compaction_id', () => {
    const onCompactingFailed = vi.fn();
    handleHarnxCustomEvent(
      'session_compacting_failed',
      { error: 'Compaction failed: no messages', compaction_id: 'compact-123' },
      { ...callbacks(true), onCompactingFailed },
    );

    expect(onCompactingFailed).toHaveBeenCalledWith('Compaction failed: no messages', 'compact-123');
  });

  it('uses default error message for failed event', () => {
    const onCompactingFailed = vi.fn();
    handleHarnxCustomEvent(
      'session_compacting_failed',
      {},
      { ...callbacks(true), onCompactingFailed },
    );

    expect(onCompactingFailed).toHaveBeenCalledWith('Compaction failed', undefined);
  });

  it('is included in NAVIGATION_CONTROL_EVENTS to prevent assistant-ui forwarding', () => {
    expect(NAVIGATION_CONTROL_EVENTS).toContain('session_compaction');
  });

  it('calls onSessionCompaction with valid compaction metadata', () => {
    const onSessionCompaction = vi.fn();
    handleHarnxCustomEvent(
      'session_compaction',
      {
        compaction: {
          message_id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
          compaction_seq: 5,
          summary: 'The session log stores tool calls and results separately. No messages were kept.',
          from_seq: 1,
          to_seq: 4,
          kept_count: 0,
        },
      },
      { ...callbacks(true), onSessionCompaction },
    );

    expect(onSessionCompaction).toHaveBeenCalledWith({
      message_id: 'a0b1fe9f-c77c-568a-8ffc-7272dd4a5cc3',
      compaction_seq: 5,
      summary: 'The session log stores tool calls and results separately. No messages were kept.',
      from_seq: 1,
      to_seq: 4,
      kept_count: 0,
    });
  });

  it('calls onSessionCompaction with null when compaction is null (clearing metadata)', () => {
    const onSessionCompaction = vi.fn();
    handleHarnxCustomEvent(
      'session_compaction',
      { compaction: null },
      { ...callbacks(true), onSessionCompaction },
    );

    expect(onSessionCompaction).toHaveBeenCalledWith(null);
  });

  it('calls onSessionCompaction with null when compaction is invalid/malformed', () => {
    const onSessionCompaction = vi.fn();
    handleHarnxCustomEvent(
      'session_compaction',
      { compaction: { invalid: true } },
      { ...callbacks(true), onSessionCompaction },
    );

    expect(onSessionCompaction).toHaveBeenCalledWith(null);
  });

  it('calls onSessionCompaction with earlier_compaction field', () => {
    const onSessionCompaction = vi.fn();
    handleHarnxCustomEvent(
      'session_compaction',
      {
        compaction: {
          message_id: 'bbdf1d00-eab3-5347-91b7-d9d60d285b7f',
          compaction_seq: 15,
          summary: 'Earlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.',
          from_seq: 6,
          to_seq: 14,
          earlier_compaction: 5,
        },
      },
      { ...callbacks(true), onSessionCompaction },
    );

    expect(onSessionCompaction).toHaveBeenCalledWith({
      message_id: 'bbdf1d00-eab3-5347-91b7-d9d60d285b7f',
      compaction_seq: 15,
      summary: 'Earlier log inspection is archived. The latest prompt, verification round and final reply remain verbatim.',
      from_seq: 6,
      to_seq: 14,
      earlier_compaction: 5,
    });
  });
});
