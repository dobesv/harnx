import { describe, it, expect, vi, beforeEach } from 'vitest';
import { listAgents, listSessions, getAgent, cancel, uploadAttachment, newSessionId } from '../api';

const fetchMock = vi.fn();
globalThis.fetch = fetchMock as any;

describe('api.ts', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  describe('listAgents', () => {
    it('returns agents data', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ data: [{ name: 'a' }] }),
      });
      const agents = await listAgents();
      expect(agents).toEqual([{ name: 'a' }]);
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents?role=assistant');
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
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent%2FA/sessions');
    });

    it('throws if not ok', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        statusText: 'Not Found',
      });
      await expect(listSessions('agent/A')).rejects.toThrow('Failed to list sessions for agent/A: Not Found');
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
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent%2FA');
    });
  });

  describe('cancel', () => {
    it('resolves cancel result on success', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ result: { cancelled: true } }),
      });
      const result = await cancel('agent', 'session');
      expect(result).toEqual({ cancelled: true });
      expect(fetchMock).toHaveBeenCalledWith('/v1/agents/agent/sessions/session', expect.objectContaining({
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
      }));
    });

    it('treats JSON-RPC -32002 (idle) as success even on HTTP 400', async () => {
      // The real backend returns HTTP 400 with a JSON-RPC -32002 error body
      // for an idle-session cancel. The body must be parsed before the status
      // check, otherwise the idle-cancel path is unreachable.
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        json: async () => ({ error: { code: -32002 } }),
      });
      const result = await cancel('agent', 'session');
      expect(result).toEqual({ cancelled: true });
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

  describe('newSessionId', () => {
    it('returns a uuid', () => {
      expect(newSessionId()).toMatch(/^[0-9a-f]{8}-/);
    });
  });
});
