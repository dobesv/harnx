import { observedFetch } from './httpClient';
import type { JsonRpcResponse } from './types';

const API_BASE = '/v1';

export interface ApprovalRef {
  invocation_id: string;
  tool_round_seq: number;
  request_seq: number;
  tool_call_id: string;
}

export type HitlExactOutcome = 'applied' | 'already_applied' | 'conflicting' | 'stale';

export interface HitlExactDecisionResult {
  applied: boolean;
  outcome: HitlExactOutcome;
  approved?: boolean;
  note?: string | null;
}

export interface SubmitHitlDecisionExactParams {
  request: ApprovalRef;
  approved: boolean;
  note?: string;
}

export async function submitHitlDecisionExact(
  agent: string,
  session: string,
  params: SubmitHitlDecisionExactParams,
): Promise<HitlExactDecisionResult> {
  const { request, approved, note } = params;
  const rpcParams: {
    request: ApprovalRef;
    approved: boolean;
    note?: string;
  } = {
    request,
    approved,
  };
  if (note !== undefined && note.trim().length > 0) {
    rpcParams.note = note.trim();
  }

  const res = await observedFetch(
    `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/hitl_decision_exact',
        params: rpcParams,
      }),
    },
  );

  let json: JsonRpcResponse<HitlExactDecisionResult> | undefined;
  try {
    json = (await res.json()) as JsonRpcResponse<HitlExactDecisionResult>;
  } catch {
    json = undefined;
  }

  if (json?.error) {
    const code = json.error.code;
    const message = json.error.message || `RPC Error: ${code}`;
    if (code === -32601 || message.toLowerCase().includes('method not found')) {
      throw new Error("this server can't answer from here; open the child session");
    }
    throw new Error(message);
  }

  if (!res.ok) {
    throw new Error(`RPC call failed with HTTP ${res.status}`);
  }

  const result = json?.result;
  if (
    !result ||
    typeof result !== 'object' ||
    typeof result.outcome !== 'string' ||
    !['applied', 'already_applied', 'conflicting', 'stale'].includes(result.outcome)
  ) {
    throw new Error('RPC call returned malformed exact hitl decision result: missing or unrecognized outcome');
  }

  const expectedApplied = result.outcome === 'applied' || result.outcome === 'already_applied';
  if (typeof result.applied !== 'boolean' || result.applied !== expectedApplied) {
    throw new Error(
      `RPC call returned malformed exact hitl decision result: applied must be ${expectedApplied} for outcome ${result.outcome}`,
    );
  }

  if (
    (result.outcome === 'already_applied' || result.outcome === 'conflicting') &&
    typeof result.approved !== 'boolean'
  ) {
    throw new Error(
      `RPC call returned malformed exact hitl decision result: missing recorded approved for ${result.outcome}`,
    );
  }

  return result;
}
