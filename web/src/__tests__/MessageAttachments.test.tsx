import { render, screen, fireEvent } from '@testing-library/react';
import '@testing-library/jest-dom';
import { describe, expect, it } from 'vitest';
import { MessageAttachments } from '../MessageAttachments';
import { MessageAttachmentsContext } from '../MessageAttachmentsContext';

describe('MessageAttachments component', () => {
  it('renders nothing when there is no metadata for the messageId', () => {
    const { container } = render(
      <MessageAttachmentsContext.Provider
        value={{
          agent: 'test-agent',
          session: 'test-session',
          attachmentsByMessageId: {},
        }}
      >
        <MessageAttachments messageId="msg-nonexistent" />
      </MessageAttachmentsContext.Provider>
    );

    expect(container).toBeEmptyDOMElement();
  });

  it('renders nothing when attachments array is empty', () => {
    const { container } = render(
      <MessageAttachmentsContext.Provider
        value={{
          agent: 'test-agent',
          session: 'test-session',
          attachmentsByMessageId: {
            'msg-empty': [],
          },
        }}
      >
        <MessageAttachments messageId="msg-empty" />
      </MessageAttachmentsContext.Provider>
    );

    expect(container).toBeEmptyDOMElement();
  });

  it('renders <img> with the correctly encoded URL for agent, session, and cid', () => {
    const agent = 'agent/with/slashes';
    const session = 'session 1';
    const cid = 'cid:0123456789abcdef';

    render(
      <MessageAttachmentsContext.Provider
        value={{
          agent,
          session,
          attachmentsByMessageId: {
            'msg-1': [{ partIndex: 0, cid, kind: 'image' }],
          },
        }}
      >
        <MessageAttachments messageId="msg-1" />
      </MessageAttachmentsContext.Provider>
    );

    const img = screen.getByRole('img');
    expect(img).toBeInTheDocument();
    expect(img).toHaveAttribute(
      'src',
      `/v1/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}/attachments/${encodeURIComponent(cid)}`
    );
    expect(img).toHaveAttribute(
      'src',
      '/v1/agents/agent%2Fwith%2Fslashes/sessions/session%201/attachments/cid%3A0123456789abcdef'
    );
    expect(img).toHaveAttribute('loading', 'lazy');
    expect(img).toHaveAttribute('alt', 'attachment');
  });

  it('renders multiple images ordered by partIndex', () => {
    render(
      <MessageAttachmentsContext.Provider
        value={{
          agent: 'test-agent',
          session: 'test-session',
          attachmentsByMessageId: {
            'msg-multi': [
              { partIndex: 2, cid: 'cid:second', kind: 'image' },
              { partIndex: 1, cid: 'cid:first', kind: 'image' },
            ],
          },
        }}
      >
        <MessageAttachments messageId="msg-multi" />
      </MessageAttachmentsContext.Provider>
    );

    const images = screen.getAllByRole('img');
    expect(images).toHaveLength(2);
    expect(images[0]).toHaveAttribute('src', expect.stringContaining('cid%3Afirst'));
    expect(images[1]).toHaveAttribute('src', expect.stringContaining('cid%3Asecond'));
    // Differentiated alt text so screen readers can tell the images apart.
    expect(images[0]).toHaveAttribute('alt', 'attachment 1 of 2');
    expect(images[1]).toHaveAttribute('alt', 'attachment 2 of 2');
    expect(screen.getByRole('group')).toHaveAttribute('aria-label', '2 message attachments');
  });

  it('renders fallback chip on onError without collapsing row', () => {
    render(
      <MessageAttachmentsContext.Provider
        value={{
          agent: 'test-agent',
          session: 'test-session',
          attachmentsByMessageId: {
            'msg-error': [{ partIndex: 0, cid: 'cid:broken', kind: 'image' }],
          },
        }}
      >
        <MessageAttachments messageId="msg-error" />
      </MessageAttachmentsContext.Provider>
    );

    const img = screen.getByRole('img');
    fireEvent.error(img);

    expect(screen.queryByRole('img')).not.toBeInTheDocument();
    const fallback = screen.getByText(/attachment unavailable/i);
    expect(fallback).toBeInTheDocument();
    expect(fallback.closest('.aui-attachment-unavailable')).toBeInTheDocument();
  });
});
