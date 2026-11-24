import { describe, expect, it, vi } from 'vitest';
import {
  submitHitlDecisionExact,
  type ApprovalRef,
} from '../api';

describe('submitHitlDecisionExact', () => {
  const dummyRequest: ApprovalRef = {
    invocation_id: 'inv-123',
    tool_round_seq: 2,
    request_seq: 5,
    tool_call_id: 'call-abc',
  };

  it('sends correct JSON-RPC payload targeting child URL and handles applied outcome', async () => {
    let capturedUrl = '';
    let capturedBody: any = null;

    vi.stubGlobal(
      'fetch',
      vi.fn().mockImplementation(async (url: string, init: any) => {
        capturedUrl = url;
        capturedBody = JSON.parse(init.body);
        return {
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: {
              applied: true,
              outcome: 'applied',
            },
          }),
        };
      }),
    );

    const result = await submitHitlDecisionExact('coder@worker-1', 'child-sess-1', {
      request: dummyRequest,
      approved: true,
      note: '  Looks safe  ',
    });

    expect(capturedUrl).toBe('/v1/agents/coder%40worker-1/sessions/child-sess-1');
    expect(capturedBody).toEqual({
      jsonrpc: '2.0',
      id: 1,
      method: 'session/hitl_decision_exact',
      params: {
        request: dummyRequest,
        approved: true,
        note: 'Looks safe',
      },
    });
    expect(result).toEqual({ applied: true, outcome: 'applied' });
  });

  it('omits empty note string', async () => {
    let capturedBody: any = null;

    vi.stubGlobal(
      'fetch',
      vi.fn().mockImplementation(async (_url: string, init: any) => {
        capturedBody = JSON.parse(init.body);
        return {
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: { applied: true, outcome: 'applied' },
          }),
        };
      }),
    );

    await submitHitlDecisionExact('coder', 'sess-1', {
      request: dummyRequest,
      approved: false,
      note: '   ',
    });

    expect(capturedBody.params).toEqual({
      request: dummyRequest,
      approved: false,
    });
  });

  it('handles already_applied, conflicting, and stale outcomes', async () => {
    // already_applied
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: true,
            outcome: 'already_applied',
            approved: true,
            note: 'recorded note',
          },
        }),
      }),
    );

    const res1 = await submitHitlDecisionExact('coder', 'sess-1', {
      request: dummyRequest,
      approved: true,
    });
    expect(res1).toEqual({
      applied: true,
      outcome: 'already_applied',
      approved: true,
      note: 'recorded note',
    });

    // conflicting
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: false,
            outcome: 'conflicting',
            approved: false,
          },
        }),
      }),
    );

    const res2 = await submitHitlDecisionExact('coder', 'sess-1', {
      request: dummyRequest,
      approved: true,
    });
    expect(res2).toEqual({
      applied: false,
      outcome: 'conflicting',
      approved: false,
    });

    // stale
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: false,
            outcome: 'stale',
          },
        }),
      }),
    );

    const res3 = await submitHitlDecisionExact('coder', 'sess-1', {
      request: dummyRequest,
      approved: true,
    });
    expect(res3).toEqual({
      applied: false,
      outcome: 'stale',
    });
  });

  it('rejects missing or unrecognized outcome (including applied: true alone)', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: true,
            // outcome missing
          },
        }),
      }),
    );

    await expect(
      submitHitlDecisionExact('coder', 'sess-1', {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow('missing or unrecognized outcome');

    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: true,
            outcome: 'unexpected_value',
          },
        }),
      }),
    );

    await expect(
      submitHitlDecisionExact('coder', 'sess-1', {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow('missing or unrecognized outcome');
  });

  it('rejects when already_applied or conflicting outcome is missing boolean approved', async () => {
    // already_applied without approved
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: true,
            outcome: 'already_applied',
          },
        }),
      }),
    );

    await expect(
      submitHitlDecisionExact('coder', 'sess-1', {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow('missing recorded approved for already_applied');

    // conflicting without approved
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          result: {
            applied: false,
            outcome: 'conflicting',
          },
        }),
      }),
    );

    await expect(
      submitHitlDecisionExact('coder', 'sess-1', {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow('missing recorded approved for conflicting');
  });

  it("rejects when applied boolean does not match outcome expectation", async () => {
    // applied: false for applied outcome
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: "2.0",
          id: 1,
          result: {
            applied: false,
            outcome: "applied",
          },
        }),
      }),
    );

    await expect(
      submitHitlDecisionExact("coder", "sess-1", {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow("applied must be true for outcome applied");

    // applied: true for conflicting outcome
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: "2.0",
          id: 1,
          result: {
            applied: true,
            outcome: "conflicting",
            approved: false,
          },
        }),
      }),
    );

    await expect(
      submitHitlDecisionExact("coder", "sess-1", {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow("applied must be false for outcome conflicting");
  });

  it('translates unknown method error to exact text without fallback', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        jsonrpc: '2.0',
        id: 1,
        error: {
          code: -32601,
          message: 'Method not found',
        },
      }),
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      submitHitlDecisionExact('coder', 'sess-1', {
        request: dummyRequest,
        approved: true,
      }),
    ).rejects.toThrow("this server can't answer from here; open the child session");

    // Strictly ensure only 1 request was made and NO fallback was executed!
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});
