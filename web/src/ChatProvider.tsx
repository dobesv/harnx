import { CancellationContext } from './CancellationContext';
import { useCancellation } from './useCancellation';
import React, { useCallback, useEffect, useMemo, useReducer, useState } from 'react';
import { AssistantRuntimeProvider } from '@assistant-ui/react';
import type { AttachmentAdapter } from '@assistant-ui/react';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { HttpAgent } from '@ag-ui/client';
import type { AgentSubscriber, Message } from '@ag-ui/client';
import { PendingContext, type HydratedPendingApproval } from './PendingContext';
import { UsageContext, type UsageData } from './UsageContext';
import { uploadAttachment } from './api';
import { RuntimeSessionSubscriber } from './RuntimeSessionSubscriber';
import { handleHarnxCustomEvent, NAVIGATION_CONTROL_EVENTS } from './harnxCustomEvents';
import { SubAgentNotesContext } from './SubAgentNotesContext';
import { INITIAL_SUB_AGENT_NOTES_STATE, reduceSubAgentNotes } from './subAgentNotes';
import { isAbortError, observedFetch } from './httpClient';
import { connection } from './connection';

export interface ChatProviderProps {
  agentName: string;
  sessionId: string;
  isFreshSession: boolean;
  onHandoff?: (agent: string, sessionId: string) => void;
  onOpenSubAgent: (agent: string, sessionId: string) => void;
  onReadUpdated?: () => void;
  children: React.ReactNode;
}

const EMPTY_STATE = {};

export interface AttachmentPart {
  type: string;
  image?: string;
  data?: string;
  mimeType?: string;
  filename?: string;
}

export interface Attachment {
  type?: string;
  name?: string;
  content?: AttachmentPart[];
}

type MessagePart =
  | { type: 'image'; image: string; filename?: string }
  | { type: 'file'; data: string; mimeType: string; filename?: string };

// eslint-disable-next-line react-refresh/only-export-components
export function attachmentToMessageParts(attachment: Attachment): MessagePart[] {
  if (!attachment.content || attachment.content.length === 0) return [];

  return attachment.content.flatMap((part) => {
    if (part.type === 'image' && typeof part.image === 'string') {
      return [{ type: 'image', image: part.image, filename: attachment.name ?? part.filename } as MessagePart];
    }

    if (part.type === 'file' && typeof part.data === 'string' && typeof part.mimeType === 'string') {
      return [{ type: 'file', data: part.data, mimeType: part.mimeType, filename: attachment.name ?? part.filename } as MessagePart];
    }

    return [];
  });
}

// Pass assistant-ui messages through to the AG-UI RunAgentInput, folding any
// uploaded attachments (cid: refs) into the user message content parts. We do NOT
// flatten multi-part content — that would drop attachments and rich content.
// eslint-disable-next-line react-refresh/only-export-components
export function toAgUiMessages(messages: readonly Message[]): Message[] {
  return messages
    .filter((message) => message.role !== 'activity')
    .map((message) => {
      if (message.role !== 'user') return message;

      const userMessage = message as Message & { attachments?: Attachment[] };
      const attachmentParts = (userMessage.attachments ?? []).flatMap(attachmentToMessageParts);
      if (attachmentParts.length === 0) return message;

      const content = Array.isArray(message.content)
        ? [...message.content, ...attachmentParts]
        : typeof message.content === 'string'
          ? [{ type: 'text', text: message.content }, ...attachmentParts]
          : message.content == null
            ? attachmentParts
            : message.content;
      return { ...message, content } as any;
    });
}

export interface HarnxHttpAgentOptions {
  url: string;
  onStatus: (text: string | null) => void;
  onRunFailed: (message: string) => void;
  onUsage: (usage: UsageData) => void;
  onToolSummary: (id: string, summary: string) => void;
  onHandoff?: (agent: string, sessionId: string) => void;
  onSubAgentEvent: (event: unknown) => void;
  onHitlPendingApproval?: (toolCallId: string, summary: string) => void;
}

const TRANSPORT_ERROR_PATTERN =
  /Failed to fetch|NetworkError|net::ERR_|connection refused|connection reset|socket|HTTP 5\d\d/i;

function isBenignRunAbort(error: unknown, message: string): boolean {
  return isAbortError(error) || isAbortError(message);
}

// Classifies a run failure as a transport (network) problem. Only drives the
// connection.noteTransientTrouble() telemetry signal — not user-facing error
// display or RUN_ERROR emission — so matching the pattern against both the
// Error and the bare message string is intentional and harmless.
function isTransportFailure(error: unknown, message: string): boolean {
  if (error instanceof TypeError) return true;
  if (error instanceof Error && TRANSPORT_ERROR_PATTERN.test(error.message)) return true;
  return TRANSPORT_ERROR_PATTERN.test(message);
}

// eslint-disable-next-line react-refresh/only-export-components
export class HarnxHttpAgent extends HttpAgent {
  private readonly onStatus: (text: string | null) => void;
  private readonly onRunFailedCb: (message: string) => void;
  private readonly onUsageCb: (usage: UsageData) => void;
  private readonly onToolSummaryCb: (id: string, summary: string) => void;
  private readonly onHandoff?: (agent: string, sessionId: string) => void;
  private readonly onSubAgentEvent: (event: unknown) => void;
  private readonly onHitlPendingApproval?: (toolCallId: string, summary: string) => void;
  private handoffBoundarySeq?: number;

  constructor(options: HarnxHttpAgentOptions) {
    super({ url: options.url, fetch: observedFetch });
    this.onStatus = options.onStatus;
    this.onRunFailedCb = options.onRunFailed;
    this.onUsageCb = options.onUsage;
    this.onToolSummaryCb = options.onToolSummary;
    this.onHandoff = options.onHandoff;
    this.onSubAgentEvent = options.onSubAgentEvent;
    this.onHitlPendingApproval = options.onHitlPendingApproval;
  }

  private handleCustomEvent(name: string, value: unknown) {
    handleHarnxCustomEvent(name, value, {
      onStatus: this.onStatus,
      onRunFailed: this.onRunFailedCb,
      onUsage: this.onUsageCb,
      onToolSummary: this.onToolSummaryCb,
      onAttachBoundary: (seq: number) => {
        this.handoffBoundarySeq = Math.max(this.handoffBoundarySeq ?? 0, seq);
      },
      onHandoff: (agent: string, sessionId: string, afterSeq: number) => {
        if (this.handoffBoundarySeq !== undefined && afterSeq > this.handoffBoundarySeq) {
          this.handoffBoundarySeq = afterSeq;
          this.onHandoff?.(agent, sessionId);
        }
      },
      onHitlPendingApproval: this.onHitlPendingApproval,
    });
  }

  private handleAgentEvent(event: any) {
    this.onSubAgentEvent(event);
    if (event?.type === 'CUSTOM') {
      this.handleCustomEvent(event.name, event.value);
    }
  }

  private handleRunFailure(message: string, error?: unknown) {
    if (isBenignRunAbort(error, message)) {
      return;
    }
    this.onSubAgentEvent({ type: 'RUN_ERROR' });
    this.onRunFailedCb(message || 'Failed to send message');

    if (isTransportFailure(error, message)) {
      connection.noteTransientTrouble();
    }
  }

  // Navigation control events are handled via onEvent (see handleAgentEvent) and
  // must not reach the assistant-ui runtime, which would synthesize phantom
  // transcript rows for them. Every other CUSTOM event forwards downstream.
  private forwardsCustomEvent(name: unknown): boolean {
    return (
      typeof name !== 'string' ||
      !(NAVIGATION_CONTROL_EVENTS as readonly string[]).includes(name)
    );
  }

  private wrapSubscriber(subscriber?: AgentSubscriber): AgentSubscriber {
    return {
      ...subscriber,
      onEvent: async (payload) => {
        this.handleAgentEvent(payload.event);
        return subscriber?.onEvent?.(payload as any);
      },
      onCustomEvent: async (payload) => {
        if (!this.forwardsCustomEvent((payload?.event as any)?.name)) return;
        return subscriber?.onCustomEvent?.(payload as any);
      },
      onRunFailed: async (payload) => {
        this.handleRunFailure(payload.error.message, payload.error);
        return subscriber?.onRunFailed?.(payload as any);
      },
    };
  }

  override async runAgent(params: any, subscriber?: AgentSubscriber) {
    const wrappedSubscriber = this.wrapSubscriber(subscriber);

    const nextParams = {
      ...params,
      state: params?.state ?? EMPTY_STATE,
      messages: toAgUiMessages(params?.messages ?? []),
    };

    try {
      return await super.runAgent(nextParams, wrappedSubscriber);
    } catch (err: any) {
      this.handleRunFailure(err.message, err);
      throw err;
    }
  }
}

export const ChatProvider: React.FC<ChatProviderProps> = ({
  agentName,
  sessionId,
  isFreshSession,
  onHandoff,
  onOpenSubAgent,
  onReadUpdated,
  children,
}) => {
  const cancellation = useCancellation(agentName, sessionId);
  const [statusText, setStatusText] = useState<string | null>(null);
  const [errorText, setErrorText] = useState<string | null>(null);
  const [usage, setUsage] = useState<UsageData | null>(null);
  const [toolSummaries, setToolSummaries] = useState<Map<string, string>>(new Map());
  const [subAgentState, dispatchSubAgentEvent] = useReducer(
    reduceSubAgentNotes,
    INITIAL_SUB_AGENT_NOTES_STATE,
  );
  // Hydrated HITL pending approvals from durable log (hitl_pending_approval CUSTOM events)
  const [hydratedApprovals, setHydratedApprovals] = useState<HydratedPendingApproval[]>([]);
  
  const addHydratedApproval = useCallback((approval: HydratedPendingApproval) => {
    setHydratedApprovals((prev) => {
      // Each tool call has at most one hydrated pending approval.
      if (prev.some((a) => a.toolCallId === approval.toolCallId)) return prev;
      return [...prev, approval];
    });
  }, []);
  
  const clearHydratedApprovals = useCallback(() => {
    setHydratedApprovals([]);
  }, []);

  const removeHydratedApproval = useCallback((toolCallId: string) => {
    setHydratedApprovals((prev) => prev.filter(a => a.toolCallId !== toolCallId));
  }, []);

  // assistant-ui 0.15.18 attachment lifecycle (verified against base-composer-runtime-core.ts):
  // - addAttachment() calls adapter.add() → status 'running', NO upload yet
  // - composerRuntime.send() calls adapter.send() for each incomplete attachment → upload + CID
  // Code paths that bypass composerRuntime.send() (e.g., out-of-band session/prompt)
  // must upload attachments themselves and cannot gate on status==='complete'.
  const attachments: AttachmentAdapter = useMemo(() => ({
    accept: 'image/png,image/jpeg,image/webp,image/gif,application/pdf,text/plain',
    async add({ file }) {
      return {
        id: crypto.randomUUID(),
        type: file.type.startsWith('image/') ? 'image' : 'file',
        name: file.name,
        contentType: file.type,
        file,
        status: {
          type: 'running',
          reason: 'uploading',
          progress: 0,
        },
      } as any;
    },
    async send(attachment) {
      try {
        const refs = await uploadAttachment(agentName, sessionId, attachment.file as File);
        const cid = refs[0];
        if (!cid) {
          // A successful upload with no cid ref would produce a malformed
          // message part (image/data: undefined) and silently lose the file.
          throw new Error('Attachment upload returned no reference');
        }
        const fileName = attachment.name ?? (attachment.file as File).name;
        const contentType = attachment.contentType ?? (attachment.file as File).type;
        return {
          ...attachment,
          contentType,
          status: { type: 'complete' },
          content: attachment.type === 'image'
            ? [{ type: 'image', image: cid, filename: fileName }]
            : [{ type: 'file', data: cid, mimeType: contentType || 'application/octet-stream', filename: fileName }],
        } as any;
      } catch (err: any) {
        throw new Error(err.message);
      }
    },
    async remove() {}
  }), [agentName, sessionId]);

  const observeCancellation = cancellation.observe;
  const agent = useMemo(() => new HarnxHttpAgent({
    url: `/v1/agents/${encodeURIComponent(agentName)}/sessions/${encodeURIComponent(sessionId)}`,
    onStatus: (text) => setStatusText(text),
    onRunFailed: (message) => setErrorText(message),
    onUsage: (newUsage) => setUsage(newUsage),
    onToolSummary: (id, summary) => {
      if (id && summary) {
        setToolSummaries((prev) => {
          const next = new Map(prev);
          next.set(id, summary);
          return next;
        });
      }
    },
    onHandoff,
    onHitlPendingApproval: (toolCallId, summary) =>
      addHydratedApproval({ toolCallId, summary }),
    onSubAgentEvent: (event: any) => {
      dispatchSubAgentEvent(event);
      if (event?.type === 'CUSTOM' && event.name === 'cancellation_state') observeCancellation(event.value.cancellation);
    },
  }), [agentName, sessionId, onHandoff, addHydratedApproval, observeCancellation]);

  const runtime = useAgUiRuntime({
    agent,
    adapters: {
      attachments,
    }
  });

  useEffect(() => {
    setStatusText(null);
    setErrorText(null);
    dispatchSubAgentEvent({ type: 'RESET' });
    clearHydratedApprovals();
  }, [agentName, sessionId, clearHydratedApprovals]);

  const subAgentContext = useMemo(() => ({
    notes: subAgentState.notes,
    openSession: onOpenSubAgent,
    dispatch: dispatchSubAgentEvent,
  }), [onOpenSubAgent, subAgentState.notes, dispatchSubAgentEvent]);

  return (
    <CancellationContext.Provider value={cancellation}>
    <SubAgentNotesContext.Provider value={subAgentContext}>
      <PendingContext.Provider value={{ 
        statusText, setStatusText, errorText, setErrorText,
        hydratedApprovals, addHydratedApproval, clearHydratedApprovals, removeHydratedApproval
      }}>
        <UsageContext.Provider value={{ usage, toolSummaries }}>
          <AssistantRuntimeProvider key={`${agentName}:${sessionId}`} runtime={runtime}>
            {/* Passive listener for existing sessions: when !isFreshSession, we follow the
                session-updated stream and hydrate without re-executing. The first message
                of a fresh session uses streaming runAgent (composerRuntime.send). */}
            <RuntimeSessionSubscriber
              enabled={!isFreshSession}
              eventsUrl={`/v1/agents/${encodeURIComponent(agentName)}/sessions/${encodeURIComponent(sessionId)}/events`}
              onReadUpdated={onReadUpdated}
            />
            {children}
          </AssistantRuntimeProvider>
        </UsageContext.Provider>
      </PendingContext.Provider>
    </SubAgentNotesContext.Provider>
    </CancellationContext.Provider>
  );
};
