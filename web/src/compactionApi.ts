import type { JsonRpcResponse } from './types';
import { observedFetch } from './httpClient';

const API_BASE = '/v1';
export const COMPACTION_TIMEOUT_MS = 15000;

export interface CompactOutcome {
  status: 'compacted' | 'unchanged' | 'failed';
  detail?: string;
}

export type CompactSubmit =
  | { status: 'submitted'; compaction_id: string }
  | { status: 'already_in_flight'; compaction_id: string }
  | { status: 'nothing_to_do'; outcome: CompactOutcome };

function createTimeoutSignal(timeoutMs: number, callerSignal?: AbortSignal): AbortSignal {
  const timeoutSignal = AbortSignal.timeout(timeoutMs);
  return callerSignal ? AbortSignal.any([callerSignal, timeoutSignal]) : timeoutSignal;
}

async function safeParseJson<T>(res: Response): Promise<JsonRpcResponse<T> | undefined> {
  try {
    return (await res.json()) as JsonRpcResponse<T>;
  } catch {
    return undefined;
  }
}

async function parseCompactResult(res: Response): Promise<CompactSubmit> {
  const json = await safeParseJson<CompactSubmit>(res);
  if (json && json.error) {
    const message = json.error.message || String(json.error.code);
    throw new Error(`RPC Error: ${message}`);
  }
  if (!res.ok) {
    throw new Error(`RPC call failed with HTTP ${res.status}`);
  }
  return json?.result as CompactSubmit;
}

export async function compactSession(
  agent: string,
  session: string,
  options?: { signal?: AbortSignal }
): Promise<CompactSubmit> {
  const res = await observedFetch(
    `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`,
    {
      method: 'POST',
      signal: createTimeoutSignal(COMPACTION_TIMEOUT_MS, options?.signal),
      headers: {
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/compact',
      }),
    }
  );

  return parseCompactResult(res);
}

/** Format UnchangedReason snake_case values for display. */
export function formatUnchangedReason(detail: string | undefined): string {
  switch (detail) {
    case 'no_user_messages':
      return 'No user messages to compact';
    case 'nothing_eligible':
      return 'Nothing eligible for compaction';
    case 'already_compacted':
      return 'Session already compacted';
    default:
      return detail || 'Nothing to compact';
  }
}
