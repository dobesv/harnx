import { describe, expect, it } from 'vitest';
import {
  INITIAL_MESSAGE_ATTACHMENTS_STATE,
  reduceMessageAttachments,
  type MessageAttachmentsState,
} from '../messageAttachments';

describe('reduceMessageAttachments', () => {
  it('stores attachments for a messageId', () => {
    const state: MessageAttachmentsState = {
      agent: 'agent-1',
      session: 'session-1',
      attachmentsByMessageId: {},
    };

    const next = reduceMessageAttachments(state, {
      type: 'SET_ATTACHMENTS',
      agent: 'agent-1',
      session: 'session-1',
      messageId: 'msg-1',
      attachments: [{ partIndex: 1, cid: 'cid:sha1', kind: 'image' }],
    });

    expect(next.attachmentsByMessageId['msg-1']).toEqual([
      { partIndex: 1, cid: 'cid:sha1', kind: 'image' },
    ]);
  });

  it('replaces (not appends) attachments for the same messageId', () => {
    const state: MessageAttachmentsState = {
      agent: 'agent-1',
      session: 'session-1',
      attachmentsByMessageId: {
        'msg-1': [{ partIndex: 1, cid: 'cid:sha1', kind: 'image' }],
      },
    };

    const next = reduceMessageAttachments(state, {
      type: 'SET_ATTACHMENTS',
      agent: 'agent-1',
      session: 'session-1',
      messageId: 'msg-1',
      attachments: [
        { partIndex: 1, cid: 'cid:sha1-replaced', kind: 'image' },
        { partIndex: 2, cid: 'cid:sha2-new', kind: 'image' },
      ],
    });

    expect(next.attachmentsByMessageId['msg-1']).toEqual([
      { partIndex: 1, cid: 'cid:sha1-replaced', kind: 'image' },
      { partIndex: 2, cid: 'cid:sha2-new', kind: 'image' },
    ]);
  });

  it('retains metadata that arrives before the row renders', () => {
    // Initial state before any message row exists
    let state = INITIAL_MESSAGE_ATTACHMENTS_STATE;

    state = reduceMessageAttachments(state, {
      type: 'SET_ATTACHMENTS',
      agent: 'agent-1',
      session: 'session-1',
      messageId: 'pending-row-msg',
      attachments: [{ partIndex: 0, cid: 'cid:preload', kind: 'image' }],
    });

    expect(state.attachmentsByMessageId['pending-row-msg']).toEqual([
      { partIndex: 0, cid: 'cid:preload', kind: 'image' },
    ]);
  });

  it('resets attachments on session change', () => {
    const state: MessageAttachmentsState = {
      agent: 'agent-1',
      session: 'session-1',
      attachmentsByMessageId: {
        'msg-1': [{ partIndex: 0, cid: 'cid:sha1', kind: 'image' }],
      },
    };

    const next = reduceMessageAttachments(state, {
      type: 'RESET',
      agent: 'agent-2',
      session: 'session-2',
    });

    expect(next.agent).toBe('agent-2');
    expect(next.session).toBe('session-2');
    expect(next.attachmentsByMessageId).toEqual({});
  });

  it('ignores events from stale subscriptions', () => {
    const state: MessageAttachmentsState = {
      agent: 'active-agent',
      session: 'active-session',
      attachmentsByMessageId: {
        'msg-1': [{ partIndex: 0, cid: 'cid:active', kind: 'image' }],
      },
    };

    // Event arrives from old session
    const staleSession = reduceMessageAttachments(state, {
      type: 'SET_ATTACHMENTS',
      agent: 'active-agent',
      session: 'stale-session',
      messageId: 'msg-stale',
      attachments: [{ partIndex: 0, cid: 'cid:stale', kind: 'image' }],
    });
    expect(staleSession.attachmentsByMessageId['msg-stale']).toBeUndefined();
    expect(staleSession).toBe(state);

    // Event arrives from old agent
    const staleAgent = reduceMessageAttachments(state, {
      type: 'SET_ATTACHMENTS',
      agent: 'stale-agent',
      session: 'active-session',
      messageId: 'msg-stale',
      attachments: [{ partIndex: 0, cid: 'cid:stale', kind: 'image' }],
    });
    expect(staleAgent.attachmentsByMessageId['msg-stale']).toBeUndefined();
    expect(staleAgent).toBe(state);
  });

  it('sorts attachments by partIndex', () => {
    const state: MessageAttachmentsState = {
      agent: 'agent-1',
      session: 'session-1',
      attachmentsByMessageId: {},
    };

    const next = reduceMessageAttachments(state, {
      type: 'SET_ATTACHMENTS',
      agent: 'agent-1',
      session: 'session-1',
      messageId: 'msg-1',
      attachments: [
        { partIndex: 3, cid: 'cid:third', kind: 'image' },
        { partIndex: 1, cid: 'cid:first', kind: 'image' },
        { partIndex: 2, cid: 'cid:second', kind: 'image' },
      ],
    });

    expect(next.attachmentsByMessageId['msg-1'].map((a) => a.cid)).toEqual([
      'cid:first',
      'cid:second',
      'cid:third',
    ]);
  });
});
