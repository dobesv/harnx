import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  cancel,
  createSession,
  fetchCidContent,
  getAgent,
  getCidUrl,
  listAgents,
  listSessions,
  sendPrompt,
  uploadAttachment,
} from '../api';

const fetchMock = vi.fn();
globalThis.fetch = fetchMock as any;

describe('api.ts', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  describe('CID content', () => {
    it('builds a URL with the complete CID encoded as one path segment', () => {
      expect(getCidUrl('cid:plan:pantheon%2Fatlas/abc123/project/tasks/t-1')).toBe(
        '/v1/cid/cid%3Aplan%3Apantheon%252Fatlas%2Fabc123%2Fproject%2Ftasks%2Ft-1',
      );
    });

    it('fetches text content with MIME type and ETag', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        status: 200,
        headers: new Headers({
          'content-type': 'text/markdown; charset=utf-8',
          etag: '"revision-7"',
        }),
        text: async () => '# Plan',
      });

      await expect(fetchCidContent('cid:plan:_temp/abc123/project')).resolves.toEqual({
        mimeType: 'text/markdown; charset=utf-8',
        text: '# Plan',
        etag: '"revision-7"',
      });
      expect(fetchMock).toHaveBeenCalledWith(
        '/v1/cid/cid%3Aplan%3A_temp%2Fabc123%2Fproject',
        undefined,
      );
    });

    it('reports an HTTP fetch failure', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 404,
        statusText: 'Not Found',
        headers: new Headers(),
      });

      await expect(fetchCidContent('cid:plan:_temp/abc123/missing')).rejects.toThrow(
        'Failed to fetch CID content (404): Not Found',
      );
    });
  });

  describe('listAgents', () => {
    it('returns agents data', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ data: [{ name: 'a' }] }),
      });
      const agents = await listAgents();
      expect(agents).toEqual([{ name: 'a' }]);
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents?role=assistant', expect.any(Object));
    });
  });

  describe('listSessions', () => {
    it('returns sessions', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => [{ id: '1' }],
      });
      const sessions = await listSessions('agent/A');
      expect(sessions).toEqual([{ id: '1' }]);
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent%2FA/sessions', expect.any(Object));
    });

    it('requests paginated sessions with limit and cursor', async () => {
      const mockResult = {
        sessions: [{ session_id: 's1' }],
        next_cursor: 'cursor-123',
      };
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => mockResult,
      });
      const result = await listSessions('agent/A', { limit: 50, cursor: 'token+1==' });
      expect(result).toEqual(mockResult);
      expect(fetchMock).toHaveBeenCalledWith(
        '/v1/agents/agent%2FA/sessions?limit=50&cursor=token%2B1%3D%3D',
        expect.any(Object),
      );
    });

    it('requests first page with limit only', async () => {
      const mockResult = {
        sessions: [{ session_id: 's1' }],
        next_cursor: null,
      };
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => mockResult,
      });
      const result = await listSessions('agent/A', { limit: 50 });
      expect(result).toEqual(mockResult);
      expect(fetchMock).toHaveBeenCalledWith(
        '/v1/agents/agent%2FA/sessions?limit=50',
        expect.any(Object),
      );
    });

    it('throws if not ok', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 404,
        statusText: 'Not Found',
      });
      await expect(listSessions('agent/A')).rejects.toThrow('Failed to list sessions for agent/A: HTTP error (404): Not Found');
    });

    it('surfaces the server explanation when session discovery is unavailable', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        statusText: 'Bad Request',
        json: async () => ({
          error: { message: 'Session discovery is not available yet; try again shortly' },
        }),
      });

      await expect(listSessions('agent/A')).rejects.toThrow(
        'Failed to list sessions for agent/A: HTTP error (400): Session discovery is not available yet; try again shortly',
      );
    });
  });

  describe('createSession', () => {
    it('returns the canonical session reserved by the backend', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ session_id: 'aok2Gw' }),
      });
      await expect(createSession('agent/A')).resolves.toEqual({ session_id: 'aok2Gw' });
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent%2FA/sessions', {
        method: 'POST',
      });
    });

    it('throws if the reservation fails', async () => {
      fetchMock.mockResolvedValueOnce({ ok: false, statusText: 'Unavailable' });
      await expect(createSession('agent/A')).rejects.toThrow(
        'Failed to create session for agent/A: Unavailable',
      );
    });
  });

  describe('getAgent', () => {
    it('returns agent detail', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ name: 'test' }),
      });
      const agent = await getAgent('agent/A');
      expect(agent).toEqual({ name: 'test' });
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent%2FA', expect.any(Object));
    });
  });

  describe('cancel', () => {
    it('resolves cancel result on success', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ result: { outcome: 'accepted', cancel_seq: 12 } }),
      });
      const result = await cancel('agent', 'session');
      expect(result).toEqual({ outcome: 'accepted', cancel_seq: 12 });
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent/sessions/session', expect.objectContaining({
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
      }));
    });

    it('treats legacy JSON-RPC -32002 (idle) as success even on HTTP 400', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        json: async () => ({ error: { code: -32002 } }),
      });
      const result = await cancel('agent', 'session');
      expect(result).toEqual({ outcome: 'idle' });
    });

    it('throws on other JSON-RPC errors', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ error: { code: -32000, message: 'Server error' } }),
      });
      await expect(cancel('agent', 'session')).rejects.toThrow('RPC Error: Server error');
    });

    it('throws on HTTP failure with no JSON-RPC error body', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 500,
        json: async () => { throw new Error('no body'); },
      });
      await expect(cancel('agent', 'session')).rejects.toThrow('RPC call failed with HTTP 500');
    });
  });

  describe('uploadAttachment', () => {
    it('returns attachment_refs', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ attachment_refs: ['cid:1'] }),
      });
      const result = await uploadAttachment('a', 's', new File(['test'], 'test.txt'));
      expect(result).toEqual(['cid:1']);
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/a/sessions/s/attachments', expect.objectContaining({
        method: 'POST',
      }));
    });

    it('parses error body json', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        statusText: 'Bad Request',
        json: async () => ({ error: 'File too large' }),
      });
      await expect(uploadAttachment('a', 's', new File([''], 't'))).rejects.toThrow('Upload failed (400): File too large');
    });

    it('falls back to statusText if no json body', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 500,
        statusText: 'Internal Server Error',
        json: async () => { throw new Error('Not JSON'); },
      });
      await expect(uploadAttachment('a', 's', new File([''], 't'))).rejects.toThrow('Upload failed (500): Internal Server Error');
    });
  });


  describe('sendPrompt', () => {
    it('resolves on success and formats envelope correctly without attachments', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ result: { status: 'accepted', run_id: 'r1' } }),
      });
      const result = await sendPrompt('agent1', 'session1', { text: 'hello' });
      expect(result).toEqual({ status: 'accepted', run_id: 'r1' });
      
      const callArgs = fetchMock.mock.calls[0];
      expect(callArgs[0]).toBe('/v1/agents/agent1/sessions/session1');
      expect(callArgs[1].method).toBe('POST');
      expect(callArgs[1].headers).toEqual({ 'Content-Type': 'application/json' });
      expect(JSON.parse(callArgs[1].body)).toEqual({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/prompt',
        params: {
          text: 'hello',
          attachment_refs: []
        }
      });
    });

    it('formats envelope correctly with attachments', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ result: { status: 'enqueued', run_id: 'r2' } }),
      });
      const result = await sendPrompt('agent1', 'session1', { text: 'hello', attachmentRefs: ['cid:x', 'cid:y'] });
      expect(result).toEqual({ status: 'enqueued', run_id: 'r2' });
      
      const callArgs = fetchMock.mock.calls[0];
      expect(JSON.parse(callArgs[1].body)).toEqual({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/prompt',
        params: {
          text: 'hello',
          attachment_refs: ['cid:x', 'cid:y']
        }
      });
    });

    it('throws on JSON-RPC error', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ error: { code: -32600, message: 'Invalid request' } }),
      });
      await expect(sendPrompt('agent1', 'session1', { text: 'hi' })).rejects.toThrow('RPC Error: Invalid request');
    });

    it('throws on HTTP failure with no JSON-RPC error body', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 500,
        json: async () => { throw new Error('no body'); },
      });
      await expect(sendPrompt('agent', 'session', { text: 'hi' })).rejects.toThrow('RPC call failed with HTTP 500');
    });
  });});
