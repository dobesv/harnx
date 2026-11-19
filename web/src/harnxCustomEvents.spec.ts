import { afterEach, describe, expect, it, vi } from 'vitest';
import { handleHarnxCustomEvent, NAVIGATION_CONTROL_EVENTS, parsePendingInteraction, parseA2uiSurface, type HarnxCustomEventCallbacks } from './harnxCustomEvents';

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
    expect(NAVIGATION_CONTROL_EVENTS).toEqual(
      expect.arrayContaining([
        'session_compaction',
        'session_compacting_started',
        'session_compacting_completed',
        'session_compacting_failed',
      ]),
    );
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


describe('pending_interaction events and parsing', () => {
  it('is included in NAVIGATION_CONTROL_EVENTS', () => {
    expect(NAVIGATION_CONTROL_EVENTS).toContain('pending_interaction');
  });

  describe('parsePendingInteraction', () => {
    it('parses a valid pending_interaction object', () => {
      const parsed = parsePendingInteraction({
        interaction_id: 'int-123',
        request_seq: 42,
        tool_call_id: 'tool-call-abc',
        kind: 'confirm',
        message: 'Execute dangerous command?',
        pending_count: 2,
      });

      expect(parsed).toEqual({
        interactionId: 'int-123',
        requestSeq: 42,
        toolCallId: 'tool-call-abc',
        kind: 'confirm',
        message: 'Execute dangerous command?',
        pendingCount: 2,
      });
    });

    it('returns null when value is null (authoritative clear signal)', () => {
      expect(parsePendingInteraction(null)).toBeNull();
    });

    it('returns undefined for non-objects', () => {
      expect(parsePendingInteraction(undefined)).toBeUndefined();
      expect(parsePendingInteraction(123)).toBeUndefined();
      expect(parsePendingInteraction('hello')).toBeUndefined();
    });

    it('returns undefined when required fields are missing or invalid', () => {
      // Missing interaction_id
      expect(
        parsePendingInteraction({
          request_seq: 1,
          tool_call_id: 'tool-1',
          kind: 'confirm',
          message: 'msg',
          pending_count: 0,
        }),
      ).toBeUndefined();

      // Blank interaction_id
      expect(
        parsePendingInteraction({
          interaction_id: '  ',
          request_seq: 1,
          tool_call_id: 'tool-1',
          kind: 'confirm',
          message: 'msg',
          pending_count: 0,
        }),
      ).toBeUndefined();

      expect(
        parsePendingInteraction({
          interaction_id: 'int-1',
          request_seq: 0,
          tool_call_id: 'tool-1',
          kind: 'confirm',
          message: 'msg',
          pending_count: 0,
        }),
      ).toBeUndefined();

      // Negative request_seq
      expect(
        parsePendingInteraction({
          interaction_id: 'int-1',
          request_seq: -1,
          tool_call_id: 'tool-1',
          kind: 'confirm',
          message: 'msg',
          pending_count: 0,
        }),
      ).toBeUndefined();

      // Non-integer request_seq
      expect(
        parsePendingInteraction({
          interaction_id: 'int-1',
          request_seq: 1.5,
          tool_call_id: 'tool-1',
          kind: 'confirm',
          message: 'msg',
          pending_count: 0,
        }),
      ).toBeUndefined();

      // Negative pending_count
      expect(
        parsePendingInteraction({
          interaction_id: 'int-1',
          request_seq: 1,
          tool_call_id: 'tool-1',
          kind: 'confirm',
          message: 'msg',
          pending_count: -1,
        }),
      ).toBeUndefined();
    });
  });

  describe('handleHarnxCustomEvent pending_interaction', () => {
    it('dispatches parsed interaction to onPendingInteraction callback', () => {
      const onPendingInteraction = vi.fn();
      handleHarnxCustomEvent(
        'pending_interaction',
        {
          interaction_id: 'int-456',
          request_seq: 10,
          tool_call_id: 'tool-xyz',
          kind: 'confirm',
          message: 'Allow file modification?',
          pending_count: 0,
        },
        { ...callbacks(true), onPendingInteraction },
      );

      expect(onPendingInteraction).toHaveBeenCalledWith({
        interactionId: 'int-456',
        requestSeq: 10,
        toolCallId: 'tool-xyz',
        kind: 'confirm',
        message: 'Allow file modification?',
        pendingCount: 0,
      });
    });

    it('dispatches null to onPendingInteraction callback when value is null', () => {
      const onPendingInteraction = vi.fn();
      handleHarnxCustomEvent(
        'pending_interaction',
        null,
        { ...callbacks(true), onPendingInteraction },
      );

      expect(onPendingInteraction).toHaveBeenCalledWith(null);
    });

    it('does not dispatch onPendingInteraction when value is malformed', () => {
      const onPendingInteraction = vi.fn();
      handleHarnxCustomEvent(
        'pending_interaction',
        { invalid: true },
        { ...callbacks(true), onPendingInteraction },
      );

      expect(onPendingInteraction).not.toHaveBeenCalled();
    });
  });

  describe('a2ui_surface events and parsing', () => {
    it('is included in NAVIGATION_CONTROL_EVENTS', () => {
      expect(NAVIGATION_CONTROL_EVENTS).toContain('a2ui_surface');
    });

    describe('parseA2uiSurface', () => {
      it('parses a publication event', () => {
        const payload = {
          version: '1.0',
          createSurface: { surfaceId: 'surf-1', components: [] },
        };
        const parsed = parseA2uiSurface({
          surface_id: 'surf-1',
          publish_seq: 7,
          tool_call_id: 'call-1',
          payload,
        });

        expect(parsed).toEqual({
          surfaceId: 'surf-1',
          publishSeq: 7,
          toolCallId: 'call-1',
          payload,
        });
      });

      it('parses a resolution event with string action', () => {
        const parsed = parseA2uiSurface({
          surface_id: 'surf-1',
          publish_seq: 7,
          action: 'deploy',
          visibility: 'note',
          note_text: 'deployed to prod',
        });

        expect(parsed).toEqual({
          surfaceId: 'surf-1',
          publishSeq: 7,
          action: {
            action: 'deploy',
            visibility: 'note',
            noteText: 'deployed to prod',
          },
        });
      });

      it('parses a resolution event with object action', () => {
        const parsed = parseA2uiSurface({
          surface_id: 'surf-1',
          publish_seq: 7,
          tool_call_id: 'call-1',
          payload: { version: '1.0' },
          action: {
            action: 'confirm',
            visibility: 'turn',
            note_text: 'confirmed',
          },
        });

        expect(parsed).toEqual({
          surfaceId: 'surf-1',
          publishSeq: 7,
          toolCallId: 'call-1',
          payload: { version: '1.0' },
          action: {
            action: 'confirm',
            visibility: 'turn',
            noteText: 'confirmed',
          },
        });
      });

      it('handles null action', () => {
        const parsed = parseA2uiSurface({
          surface_id: 'surf-1',
          publish_seq: 7,
          payload: { version: '1.0' },
          action: null,
        });

        expect(parsed).toEqual({
          surfaceId: 'surf-1',
          publishSeq: 7,
          payload: { version: '1.0' },
          action: null,
        });
      });

      it('returns undefined for invalid surface payloads', () => {
        expect(parseA2uiSurface(null)).toBeUndefined();
        expect(parseA2uiSurface({})).toBeUndefined();
        expect(parseA2uiSurface({ surface_id: '', publish_seq: 1 })).toBeUndefined();
        expect(parseA2uiSurface({ surface_id: 's', publish_seq: 0 })).toBeUndefined();
        expect(parseA2uiSurface({ surface_id: 's', publish_seq: -1 })).toBeUndefined();
        expect(parseA2uiSurface({ surface_id: 's', publish_seq: 1 })).toBeUndefined(); // no payload or action
      });
    });

    describe('handleHarnxCustomEvent a2ui_surface', () => {
      it('dispatches to onA2uiSurface callback', () => {
        const onA2uiSurface = vi.fn();
        const payload = {
          version: '1.0',
          createSurface: { surfaceId: 'surf-1', components: [] },
        };

        handleHarnxCustomEvent(
          'a2ui_surface',
          {
            surface_id: 'surf-1',
            publish_seq: 3,
            tool_call_id: 'call-99',
            payload,
          },
          { ...callbacks(true), onA2uiSurface }
        );

        expect(onA2uiSurface).toHaveBeenCalledWith({
          surfaceId: 'surf-1',
          publishSeq: 3,
          toolCallId: 'call-99',
          payload,
        });
      });

      it('ignores malformed a2ui_surface events', () => {
        const onA2uiSurface = vi.fn();
        handleHarnxCustomEvent(
          'a2ui_surface',
          { invalid: true },
          { ...callbacks(true), onA2uiSurface }
        );

        expect(onA2uiSurface).not.toHaveBeenCalled();
      });
    });
  });
});
