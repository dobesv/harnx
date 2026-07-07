import type { Agent, SessionRef, HistoryMessage, AgentDetail, JsonRpcResponse, CancelResult } from './types';

const API_BASE = '/v1';

export async function listAgents(): Promise<Agent[]> {
  const res = await fetch(`${API_BASE}/agents`);
  if (!res.ok) throw new Error(`Failed to list agents: ${res.statusText}`);
  const json = await res.json() as { data: Agent[] };
  return json.data;
}

export async function listSessions(agent: string): Promise<SessionRef[]> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions`);
  if (!res.ok) throw new Error(`Failed to list sessions for ${agent}: ${res.statusText}`);
  const json = await res.json() as SessionRef[];
  return json;
}

export async function getSessionHistory(agent: string, session: string): Promise<HistoryMessage[]> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`);
  if (!res.ok) throw new Error(`Failed to fetch history for session ${session}: ${res.statusText}`);
  const json = await res.json() as HistoryMessage[];
  return json;
}

export async function getAgent(agent: string): Promise<AgentDetail> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}`);
  if (!res.ok) throw new Error(`Failed to get agent ${agent}: ${res.statusText}`);
  const json = await res.json() as AgentDetail;
  return json;
}

export async function cancel(agent: string, session: string): Promise<CancelResult> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}/rpc`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'session/cancel'
    })
  });
  
  if (!res.ok) throw new Error(`RPC call failed with HTTP ${res.status}`);
  
  const json = await res.json() as JsonRpcResponse<CancelResult>;
  
  if (json.error) {
    if (json.error.code === -32002) {
      return { cancelled: true }; // Benign no-op
    }
    throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  }
  
  return json.result as CancelResult;
}

export function newSessionId(): string {
  return crypto.randomUUID();
}
