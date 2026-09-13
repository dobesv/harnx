import type { JsonRpcResponse, SessionControlState, CancelResult } from './types';
import { observedFetch } from './httpClient';

const API_BASE = '/v1';
export const CANCELLATION_TIMEOUT_MS = 15000;

function createTimeoutSignal(timeoutMs: number, callerSignal?: AbortSignal): AbortSignal {
  const timeoutSignal = AbortSignal.timeout(timeoutMs);
  return callerSignal ? AbortSignal.any([callerSignal, timeoutSignal]) : timeoutSignal;
}

export async function sessionControl(
  agent: string,
  session: string,
  options?: { signal?: AbortSignal }
): Promise<SessionControlState> {
  const response = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    signal: createTimeoutSignal(CANCELLATION_TIMEOUT_MS, options?.signal),
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 'control', method: 'session/get' }),
  });
  const body = await response.json() as JsonRpcResponse<SessionControlState>;
  if (!response.ok || body.error || !body.result) throw new Error(body.error?.message ?? 'Cannot load session cancellation state');
  return body.result;
}

async function safeParseJson<T>(res: Response): Promise<JsonRpcResponse<T> | undefined> {
  try {
    return (await res.json()) as JsonRpcResponse<T>;
  } catch {
    return undefined;
  }
}

function handleRpcError(error: { code: number | string; message?: string }): CancelResult {
  if (error.code === -32002) {
    return { cancelled: false, disposition: 'idle' };
  }
  const message = error.message || error.code;
  throw new Error(`RPC Error: ${message}`);
}

async function parseCancelResult(res: Response): Promise<CancelResult> {
  const json = await safeParseJson<CancelResult>(res);
  if (json && json.error) {
    return handleRpcError(json.error);
  }
  if (!res.ok) {
    throw new Error(`RPC call failed with HTTP ${res.status}`);
  }
  return json?.result as CancelResult;
}

export async function cancel(
  agent: string,
  session: string,
  expectedExecutionId?: string,
  options?: { signal?: AbortSignal }
): Promise<CancelResult> {
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    signal: createTimeoutSignal(CANCELLATION_TIMEOUT_MS, options?.signal),
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

  return parseCancelResult(res);
}

export async function abandonCancellation(
  agent: string,
  session: string,
  expectedExecutionId: string,
  options?: { signal?: AbortSignal }
): Promise<CancelResult> {
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    signal: createTimeoutSignal(CANCELLATION_TIMEOUT_MS, options?.signal),
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'session/abandon_cancellation',
      params: { expected_execution_id: expectedExecutionId }
    })
  });
  const json = await res.json() as JsonRpcResponse<CancelResult>;
  if (json.error) throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  if (!res.ok || !json.result) throw new Error(`RPC call failed with HTTP ${res.status}`);
  return json.result;
}
