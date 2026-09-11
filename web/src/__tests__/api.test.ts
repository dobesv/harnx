import { describe, it, expect, vi, beforeEach } from 'vitest';
import { listAgents, listSessions, createSession, getAgent, cancel, sessionControl, uploadAttachment, sendPrompt } from '../api';

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

    it('surfaces the server explanation when session discovery is unavailable', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        statusText: 'Bad Request',
        json: async () => ({
          error: { message: 'Session discovery is not available yet; try again shortly' },
        }),
      });

      await expect(listSessions('agent/A')).rejects.toThrow(
        'Failed to list sessions for agent/A: Session discovery is not available yet; try again shortly',
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

    it('treats legacy JSON-RPC -32002 (idle) as success even on HTTP 400', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: false,
        status: 400,
        json: async () => ({ error: { code: -32002 } }),
      });
      const result = await cancel('agent', 'session');
      expect(result).toEqual({ cancelled: false, disposition: 'idle' });
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

  describe('sessionControl', () => {
    it('loads durable cancellation state with a two-second request deadline', async () => {
      const timeoutSignal = new AbortController().signal;
      const timeoutSpy = vi.spyOn(AbortSignal, 'timeout').mockReturnValue(timeoutSignal);
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          result: {
            execution_state: 'cancel_requested',
            state: { status: 'cancelling' },
            execution_id: 'exec-1',
            canPrompt: false,
            canCancel: true,
          },
        }),
      });

      try {
        await expect(sessionControl('agent/A', 'session B')).resolves.toEqual(
          expect.objectContaining({ execution_id: 'exec-1', canPrompt: false }),
        );
        expect(timeoutSpy).toHaveBeenCalledWith(2000);
        expect(fetchMock).toHaveBeenCalledWith(
          '/v1/agents/agent%2FA/sessions/session%20B',
          expect.objectContaining({
            method: 'POST',
            signal: timeoutSignal,
            body: JSON.stringify({ jsonrpc: '2.0', id: 'control', method: 'session/get' }),
          }),
        );
      } finally {
        timeoutSpy.mockRestore();
      }
    });

    it('surfaces JSON-RPC failures', async () => {
      fetchMock.mockResolvedValueOnce({
        ok: true,
        json: async () => ({ error: { code: -32000, message: 'Control state unavailable' } }),
      });

      await expect(sessionControl('agent', 'session')).rejects.toThrow(
        'Control state unavailable',
      );
    });

    it('propagates transport timeouts', async () => {
      fetchMock.mockRejectedValueOnce(new DOMException('The operation timed out', 'TimeoutError'));

      await expect(sessionControl('agent', 'session')).rejects.toMatchObject({
        name: 'TimeoutError',
      });
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
