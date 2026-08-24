import type { Agent, SessionRef, AgentDetail, JsonRpcResponse, CancelResult } from './types';

const API_BASE = '/v1';

export async function listAgents(): Promise<Agent[]> {
  const res = await fetch(`${API_BASE}/agents?role=assistant`);
  if (!res.ok) throw new Error(`Failed to list agents: ${res.statusText}`);
  const json = await res.json() as { data: Agent[] };
  return json.data;
}

export async function listSessions(agent: string): Promise<SessionRef[]> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions`);
  if (!res.ok) {
    let detail = res.statusText;
    try {
      const body = await res.json() as { error?: { message?: string } | string };
      detail = typeof body.error === 'string' ? body.error : body.error?.message || detail;
    } catch {
      // Keep the HTTP status text when the response is not JSON.
    }
    throw new Error(`Failed to list sessions for ${agent}: ${detail}`);
  }
  const json = await res.json() as SessionRef[];
  return json;
}

export async function createSession(agent: string): Promise<SessionRef> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions`, {
    method: 'POST',
  });
  if (!res.ok) throw new Error(`Failed to create session for ${agent}: ${res.statusText}`);
  return await res.json() as SessionRef;
}

export async function getAgent(agent: string): Promise<AgentDetail> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}`);
  if (!res.ok) throw new Error(`Failed to get agent ${agent}: ${res.statusText}`);
  const json = await res.json() as AgentDetail;
  return json;
}

export async function cancel(agent: string, session: string): Promise<CancelResult> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
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

  // Parse the JSON-RPC body BEFORE checking res.ok: the backend returns
  // HTTP 400 for an idle-session cancel with a JSON-RPC error body carrying
  // code -32002, which we treat as a successful no-op. Throwing on !res.ok
  // first would make that branch unreachable.
  let json: JsonRpcResponse<CancelResult> | undefined;
  try {
    json = await res.json() as JsonRpcResponse<CancelResult>;
  } catch {
    json = undefined;
  }

  if (json?.error) {
    if (json.error.code === -32002) {
      // Cancelling an already-idle session is a benign no-op.
      return { cancelled: true };
    }
    throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  }

  if (!res.ok) throw new Error(`RPC call failed with HTTP ${res.status}`);

  return json?.result as CancelResult;
}

export async function uploadAttachment(agent: string, session: string, file: File): Promise<string[]> {
  const formData = new FormData();
  formData.append('attachment', file);
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}/attachments`, {
    method: 'POST',
    body: formData
  });
  if (!res.ok) {
    let msg = res.statusText;
    try {
      const j = await res.json();
      if (j.error) msg = j.error;
    } catch {}
    throw new Error(`Upload failed (${res.status}): ${msg}`);
  }
  const json = await res.json();
  return json.attachment_refs || [];
}
