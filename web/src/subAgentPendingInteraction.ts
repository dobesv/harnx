import type { ApprovalRef } from "./hitlExactApi";

export type { ApprovalRef };

export interface BasePendingInteractionSummary {
  agent: string;
  cluster: string;
  sessionId: string;
  toolName: string;
  message: string;
  pendingCount: number;
}

export interface LegacyApprovalPendingInteraction extends BasePendingInteractionSummary {
  supported: true;
  type: "legacy_approval";
  request: ApprovalRef;
}

export interface ConfirmPendingInteraction extends BasePendingInteractionSummary {
  supported: true;
  type: "confirm";
  interactionId: string;
  requestSeq: number;
}

export interface UnsupportedPendingInteraction extends BasePendingInteractionSummary {
  supported: false;
  type: "unsupported";
  rawKind: string;
  interactionKind?: string;
}

export type SubAgentPendingInteraction =
  | LegacyApprovalPendingInteraction
  | ConfirmPendingInteraction
  | UnsupportedPendingInteraction;

export function resolveChildAgentAddress(agent: string, cluster?: string | null): string {
  if (!cluster || cluster === "__local__") {
    return agent;
  }
  return `${agent}@${cluster}`;
}

export function getPendingInteractionKey(
  pending: SubAgentPendingInteraction | undefined,
): string | undefined {
  if (!pending) return undefined;
  if (pending.type === "legacy_approval") {
    const { invocation_id, tool_round_seq, request_seq, tool_call_id } = pending.request;
    return `legacy:${pending.cluster}:${pending.agent}:${pending.sessionId}:${invocation_id}:${tool_round_seq}:${request_seq}:${tool_call_id}`;
  }
  if (pending.type === "confirm") {
    return `confirm:${pending.cluster}:${pending.agent}:${pending.sessionId}:${pending.interactionId}:${pending.requestSeq}`;
  }
  return `unsupported:${pending.cluster}:${pending.agent}:${pending.sessionId}:${pending.rawKind}:${pending.message}`;
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function nonNegativeInteger(value: unknown): number | undefined {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : undefined;
}

function positiveInteger(value: unknown): number | undefined {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0
    ? value
    : undefined;
}

function parseApprovalRef(value: unknown): ApprovalRef | undefined {
  if (!value || typeof value !== "object") return undefined;
  const rec = value as Record<string, unknown>;
  const invocationId = nonEmptyString(rec.invocation_id);
  const toolCallId = nonEmptyString(rec.tool_call_id);
  const toolRoundSeq = nonNegativeInteger(rec.tool_round_seq);
  const requestSeq = positiveInteger(rec.request_seq);

  if (invocationId && toolCallId && toolRoundSeq !== undefined && requestSeq !== undefined) {
    return {
      invocation_id: invocationId,
      tool_round_seq: toolRoundSeq,
      request_seq: requestSeq,
      tool_call_id: toolCallId,
    };
  }
  return undefined;
}

export function parseSubAgentPendingInteraction(value: unknown): SubAgentPendingInteraction | undefined {
  if (!value || typeof value !== "object") return undefined;
  const rec = value as Record<string, unknown>;

  const agent = nonEmptyString(rec.agent);
  const sessionId = nonEmptyString(rec.session_id);
  if (!agent || !sessionId) return undefined;

  const cluster = typeof rec.cluster === "string" && rec.cluster.length > 0
    ? rec.cluster
    : "__local__";
  const toolName = typeof rec.tool_name === "string" ? rec.tool_name : "";
  const message = typeof rec.message === "string" ? rec.message : "";
  const pendingCount = nonNegativeInteger(rec.pending_count) ?? 0;

  const kind = nonEmptyString(rec.kind);
  if (!kind) return undefined;

  const base: BasePendingInteractionSummary = {
    agent,
    cluster,
    sessionId,
    toolName,
    message,
    pendingCount,
  };

  if (kind === "legacy_approval") {
    const request = parseApprovalRef(rec.request);
    if (!request) return undefined;
    return {
      ...base,
      supported: true,
      type: "legacy_approval",
      request,
    };
  }

  if (kind === "interaction") {
    const interactionId = nonEmptyString(rec.interaction_id);
    const requestSeq = positiveInteger(rec.request_seq);
    const interactionKind = nonEmptyString(rec.interaction_kind);

    if (!interactionId || requestSeq === undefined) return undefined;

    if (interactionKind === "confirm") {
      return {
        ...base,
        supported: true,
        type: "confirm",
        interactionId,
        requestSeq,
      };
    }

    return {
      ...base,
      supported: false,
      type: "unsupported",
      rawKind: kind,
      interactionKind,
    };
  }

  return {
    ...base,
    supported: false,
    type: "unsupported",
    rawKind: kind,
    interactionKind: nonEmptyString(rec.interaction_kind),
  };
}
