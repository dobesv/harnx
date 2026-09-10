import type { Agent, SessionRef, AgentDetail, JsonRpcResponse, PromptResult } from './types';

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

export async function sendPrompt(
  agent: string,
  session: string,
  { text, attachmentRefs }: { text: string; attachmentRefs?: string[] }
): Promise<PromptResult> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'session/prompt',
      params: {
        text,
        attachment_refs: attachmentRefs ?? []
      }
    })
  });

  let json: JsonRpcResponse<PromptResult> | undefined;
  try {
    json = await res.json() as JsonRpcResponse<PromptResult>;
  } catch {
    json = undefined;
  }

  if (json?.error) {
    throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  }

  if (!res.ok) throw new Error(`RPC call failed with HTTP ${res.status}`);

  return json?.result as PromptResult;
}

export async function submitHitlDecision(
  agent: string,
  session: string,
  { toolCallId, approved, note }: { toolCallId: string; approved: boolean; note?: string }
): Promise<{ applied: boolean }> {
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'session/hitl_decision',
      params: { tool_call_id: toolCallId, approved, note }
    })
  });
  let json: JsonRpcResponse<{ applied: boolean }> | undefined;
  try {
    json = await res.json() as JsonRpcResponse<{ applied: boolean }>;
  } catch {
    json = undefined;
  }
  if (json?.error) throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  if (!res.ok) throw new Error(`RPC call failed with HTTP ${res.status}`);
  return json?.result || { applied: false };
}

export { cancel, sessionControl } from './cancellationApi';
