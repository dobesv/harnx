import type { Agent, AgentDetail, JsonRpcResponse, PromptResult, SessionRef } from './types';
import { fetchJsonWithRetry, observedFetch, PermanentError } from './httpClient';

const API_BASE = '/v1';

export async function listAgents(options?: { signal?: AbortSignal }): Promise<Agent[]> {
  try {
    const json = await fetchJsonWithRetry<{ data: Agent[] }>(
      `${API_BASE}/agents?role=assistant`,
      undefined,
      options
    );
    return json.data;
  } catch (err) {
    if (err instanceof PermanentError) {
      throw new Error(`Failed to list agents: ${err.message}`);
    }
    throw err;
  }
}

export async function listSessions(agent: string, options?: { signal?: AbortSignal }): Promise<SessionRef[]> {
  try {
    const json = await fetchJsonWithRetry<SessionRef[]>(
      `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions`,
      undefined,
      options
    );
    return json;
  } catch (err) {
    if (err instanceof PermanentError) {
      throw new Error(`Failed to list sessions for ${agent}: ${err.message}`);
    }
    throw err;
  }
}

export async function getAgent(agent: string, options?: { signal?: AbortSignal }): Promise<AgentDetail> {
  try {
    const json = await fetchJsonWithRetry<AgentDetail>(
      `${API_BASE}/agents/${encodeURIComponent(agent)}`,
      undefined,
      options
    );
    return json;
  } catch (err) {
    if (err instanceof PermanentError) {
      throw new Error(`Failed to get agent ${agent}: ${err.message}`);
    }
    throw err;
  }
}

export interface CreateSessionResult {
  session_id: string;
  title?: string | null;
  updated_at?: string | number | null;
  [key: string]: unknown;
}

export async function createSession(agent: string): Promise<CreateSessionResult> {
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions`, {
    method: 'POST',
  });
  if (!res.ok) throw new Error(`Failed to create session for ${agent}: ${res.statusText}`);
  return await res.json() as CreateSessionResult;
}

export async function uploadAttachment(
  agent: string,
  session: string,
  file: File
): Promise<string[]> {
  const form = new FormData();
  form.append('file', file);
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}/attachments`, {
    method: 'POST',
    body: form,
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
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
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
  const res = await observedFetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
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
