import { sessionRpcResponse } from './sessionRpcResponse';
import type {
  Agent,
  AgentDetail,
  JsonRpcResponse,
  PaginatedSessions,
  PromptResult,
  SessionHistoryParams,
  SessionHistoryResult,
  SessionInfo,
  SessionRef,
} from './types';
import { fetchJsonWithRetry, observedFetch, PermanentError } from './httpClient';

export const API_BASE = '/v1';

export function getSessionAttachmentUrl(agent: string, session: string, cid: string): string {
  return `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}/attachments/${encodeURIComponent(cid)}`;
}

export function getCidUrl(cid: string): string {
  return `${API_BASE}/cid/${encodeURIComponent(cid)}`;
}

export async function fetchCidContent(
  cid: string,
): Promise<{ mimeType: string; text: string; etag?: string }> {
  const res = await observedFetch(getCidUrl(cid));
  if (!res.ok) {
    throw new Error(`Failed to fetch CID content (${res.status}): ${res.statusText}`);
  }

  const mimeType = res.headers.get('content-type') || 'text/plain';
  const etag = res.headers.get('etag') || undefined;
  const text = await res.text();
  return etag ? { mimeType, text, etag } : { mimeType, text };
}

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

export interface ListSessionsOptions {
  signal?: AbortSignal;
  limit?: number;
  cursor?: string;
}

function appendQueryParam(
  params: URLSearchParams,
  key: string,
  value: string | number | undefined,
): void {
  if (value !== undefined) {
    params.set(key, String(value));
  }
}

function buildSessionListQuery(options?: ListSessionsOptions): string {
  if (!options) return '';
  const params = new URLSearchParams();
  appendQueryParam(params, 'limit', options.limit);
  appendQueryParam(params, 'cursor', options.cursor);
  const query = params.toString();
  return query ? `?${query}` : '';
}

export async function listSessions(
  agent: string,
  options: ListSessionsOptions & ({ limit: number } | { cursor: string }),
): Promise<PaginatedSessions>;
export async function listSessions(
  agent: string,
  options?: ListSessionsOptions,
): Promise<SessionRef[] | PaginatedSessions>;
export async function listSessions(
  agent: string,
  options?: ListSessionsOptions,
): Promise<SessionRef[] | PaginatedSessions> {
  const query = buildSessionListQuery(options);
  const url = `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions${query}`;
  try {
    return await fetchJsonWithRetry<SessionRef[] | PaginatedSessions>(
      url,
      undefined,
      options
    );
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

  return normalizePromptResult(json?.result);
}

function normalizePromptResult(result: PromptResult | undefined): PromptResult {
  switch (result?.status) {
    case 'accepted':
    case 'enqueued':
      // A server from before message_id was added can still have accepted the
      // prompt. Preserve that success so the composer can show a local queued row.
      return { ...result, message_id: usablePromptMessageId(result.message_id) };
    default:
      throw new Error('RPC call returned no prompt acknowledgement');
  }
}

function usablePromptMessageId(value: unknown): string | null {
  // Correlation metadata cannot undo acceptance. An unusable ID gets the same
  // visible fallback as legacy metadata, never a send failure inviting replay.
  if (typeof value !== 'string') return null;
  return value.trim().length > 0 ? value : null;
}

export type InteractionResponseAction = 'approve' | 'deny' | (string & {});

export interface InteractionResponsePayload {
  action: InteractionResponseAction;
  data?: Record<string, unknown>;
  note?: string;
}

export interface SubmitInteractionResponseParams {
  interactionId: string;
  requestSeq: number;
  response: InteractionResponsePayload;
}

export type InteractionResponseOutcome = 'applied' | 'already_applied';

export interface InteractionResponseResult {
  applied: boolean;
  outcome: InteractionResponseOutcome;
}

export type InteractionRejectionOutcome =
  | 'conflicting'
  | 'wrong_sequence'
  | 'stale'
  | 'after_complete'
  | 'invalid_response'
  | 'lease_lost';

export interface RecordedInteractionResponse {
  action: string;
  data?: Record<string, unknown>;
  note?: string | null;
}

export interface InteractionRejectionData {
  outcome: InteractionRejectionOutcome;
  response?: RecordedInteractionResponse;
  [key: string]: unknown;
}

export class InteractionResponseError extends Error {
  readonly code: number;
  readonly outcome?: InteractionRejectionOutcome;
  readonly data?: InteractionRejectionData;
  readonly response?: RecordedInteractionResponse;

  constructor(
    message: string,
    code: number,
    outcome?: InteractionRejectionOutcome,
    data?: InteractionRejectionData
  ) {
    super(message);
    this.name = "InteractionResponseError";
    this.code = code;
    this.outcome = outcome;
    this.data = data;
    this.response = data?.response;
  }
}

export async function submitInteractionResponse(
  agent: string,
  session: string,
  params: SubmitInteractionResponseParams
): Promise<InteractionResponseResult> {
  const { interactionId, requestSeq, response } = params;
  const responsePayload: {
    action: InteractionResponseAction;
    data: Record<string, unknown>;
    note?: string;
  } = {
    action: response.action,
    data: response.data ?? {},
  };
  if (response.note !== undefined && response.note.trim().length > 0) {
    responsePayload.note = response.note.trim();
  }

  const res = await observedFetch(
    `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/interaction_response',
        params: {
          interaction_id: interactionId,
          request_seq: requestSeq,
          response: responsePayload,
        },
      }),
    }
  );

  let json: JsonRpcResponse<InteractionResponseResult> | undefined;
  try {
    json = (await res.json()) as JsonRpcResponse<InteractionResponseResult>;
  } catch {
    json = undefined;
  }

  if (json?.error) {
    const code = json.error.code;
    const message = json.error.message || `RPC Error: ${code}`;
    const data = json.error.data as InteractionRejectionData | undefined;
    const outcome = data?.outcome;
    throw new InteractionResponseError(message, code, outcome, data);
  }

  if (!res.ok) {
    throw new Error(`RPC call failed with HTTP ${res.status}`);
  }

  const result = json?.result;
  if (
    !result ||
    typeof result !== 'object' ||
    result.applied !== true ||
    (result.outcome !== 'applied' && result.outcome !== 'already_applied')
  ) {
    throw new Error('RPC call returned malformed interaction response result');
  }

  return result;
}

export interface SubmitSurfaceActionParams {
  surfaceId: string;
  publishSeq: number;
  response: InteractionResponsePayload;
  userMessage?: string;
}

export type SurfaceActionOutcome = 'applied' | 'already_applied';

export interface SurfaceActionResult {
  applied: boolean;
  outcome: SurfaceActionOutcome;
}

export type SurfaceActionRejectionOutcome =
  | 'conflicting'
  | 'unknown_surface'
  | 'undeclared_action'
  | 'invalid_visibility'
  | 'invalid_tool'
  | 'invalid_data'
  | 'unauthorized_tool'
  | 'tool_failed'
  | 'lease_lost';

export interface SurfaceActionRejectionData {
  outcome?: SurfaceActionRejectionOutcome | string;
  [key: string]: unknown;
}

export class SurfaceActionError extends Error {
  readonly code: number;
  readonly outcome?: SurfaceActionRejectionOutcome | string;
  readonly data?: SurfaceActionRejectionData;

  constructor(
    message: string,
    code: number,
    outcome?: SurfaceActionRejectionOutcome | string,
    data?: SurfaceActionRejectionData
  ) {
    super(message);
    this.name = 'SurfaceActionError';
    this.code = code;
    this.outcome = outcome;
    this.data = data;
  }
}

export async function submitSurfaceAction(
  agent: string,
  session: string,
  params: SubmitSurfaceActionParams
): Promise<SurfaceActionResult> {
  const { surfaceId, publishSeq, response, userMessage } = params;
  const responsePayload: {
    action: InteractionResponseAction;
    data: Record<string, unknown>;
    note?: string;
  } = {
    action: response.action,
    data: response.data ?? {},
  };
  if (response.note !== undefined && response.note.trim().length > 0) {
    responsePayload.note = response.note.trim();
  }

  const rpcParams: Record<string, unknown> = {
    surface_id: surfaceId,
    publish_seq: publishSeq,
    response: responsePayload,
  };
  if (userMessage !== undefined && userMessage.trim().length > 0) {
    rpcParams.user_message = userMessage.trim();
  }

  const res = await observedFetch(
    `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/surface_action',
        params: rpcParams,
      }),
    }
  );

  let json: JsonRpcResponse<SurfaceActionResult> | undefined;
  try {
    json = (await res.json()) as JsonRpcResponse<SurfaceActionResult>;
  } catch {
    json = undefined;
  }

  if (json?.error) {
    const code = json.error.code;
    const message = json.error.message || `RPC Error: ${code}`;
    const data = json.error.data as SurfaceActionRejectionData | undefined;
    const outcome = data?.outcome;
    throw new SurfaceActionError(message, code, outcome, data);
  }

  if (!res.ok) {
    throw new Error(`RPC call failed with HTTP ${res.status}`);
  }

  const result = json?.result;
  if (
    !result ||
    typeof result !== 'object' ||
    result.applied !== true ||
    (result.outcome !== 'applied' && result.outcome !== 'already_applied')
  ) {
    throw new Error('RPC call returned malformed surface action result');
  }

  return result;
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
  return sessionRpcResponse<{ applied: boolean }>(res, { applied: false });
}

export {
  submitHitlDecisionExact,
  type ApprovalRef,
  type HitlExactOutcome,
  type HitlExactDecisionResult,
  type SubmitHitlDecisionExactParams,
} from './hitlExactApi';

export interface SessionTarget {
  agent: string;
  session: string;
}

async function sessionRpcStatusCall(
  target: SessionTarget,
  method: string,
): Promise<{ status: string }> {
  const { agent, session } = target;
  const res = await fetch(`${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method,
    }),
  });
  return sessionRpcResponse<{ status: string }>(res, { status: 'ok' });
}

export async function markRead(
  agent: string,
  session: string,
): Promise<{ status: string }> {
  return sessionRpcStatusCall({ agent, session }, 'session/mark_read');
}

export async function markUnread(
  agent: string,
  session: string,
): Promise<{ status: string }> {
  return sessionRpcStatusCall({ agent, session }, 'session/mark_unread');
}

export { cancel, CANCELLATION_TIMEOUT_MS } from './cancellationApi';
export { compactSession, COMPACTION_TIMEOUT_MS, type CompactSubmit, type CompactOutcome } from './compactionApi';


export async function getSession(
  agent: string,
  session: string,
  options?: { signal?: AbortSignal }
): Promise<SessionInfo> {
  const res = await observedFetch(
    `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`,
    {
      method: 'POST',
      signal: options?.signal,
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/get',
      }),
    }
  );
  return sessionRpcResponse<SessionInfo>(res, {});
}

export async function getSessionHistory(
  agent: string,
  session: string,
  params: SessionHistoryParams,
  options?: { signal?: AbortSignal }
): Promise<SessionHistoryResult> {
  const res = await observedFetch(
    `${API_BASE}/agents/${encodeURIComponent(agent)}/sessions/${encodeURIComponent(session)}`,
    {
      method: 'POST',
      signal: options?.signal,
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/history',
        params,
      }),
    }
  );
  return sessionRpcResponse<SessionHistoryResult>(res, {
    history_snapshot: [],
    next_cursor: null,
    prev_cursor: null,
    earlier_compaction: null,
  });
}
