import type { JsonRpcResponse, SessionControlState, CancelResult } from './types';
import { observedFetch } from './httpClient';

const API_BASE = '/v1';

export async function sessionControl(agent: string, session: string): Promise<SessionControlState> {
  const response = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    signal: AbortSignal.timeout(2000),
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 'control', method: 'session/get' }),
  });
  const body = await response.json() as JsonRpcResponse<SessionControlState>;
  if (!response.ok || body.error || !body.result) throw new Error(body.error?.message ?? 'Cannot load session cancellation state');
  return body.result;
}

export async function cancel(agent: string, session: string, expectedExecutionId?: string): Promise<CancelResult> {
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    signal: AbortSignal.timeout(2000),
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'session/cancel',
      params: { expected_execution_id: expectedExecutionId, retry: true }
    })
  });

  // Parse the JSON-RPC body before checking res.ok so older servers can report
  // an idle-session cancellation as a successful no-op via error code -32002.
  let json: JsonRpcResponse<CancelResult> | undefined;
  try {
    json = await res.json() as JsonRpcResponse<CancelResult>;
  } catch {
    json = undefined;
  }

  if (json?.error) {
    if (json.error.code === -32002) {
      // Cancelling an already-idle session is a benign no-op.
      return { cancelled: false, disposition: 'idle' };
    }
    throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  }

  if (!res.ok) throw new Error(`RPC call failed with HTTP ${res.status}`);

  return json?.result as CancelResult;
}
