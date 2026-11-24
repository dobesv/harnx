import { CancellationContext } from './CancellationContext';
import { useCancellation } from './useCancellation';
import { CompactionContext, compactionAfterRunTerminal, type CompactionPhase } from './CompactionContext';
import { CompactionMetadataContext } from './CompactionMetadataContext';
import type { CompactionMetadata } from './compactionMetadata';
import React, { useCallback, useEffect, useMemo, useReducer, useRef, useState } from 'react';
import { AssistantRuntimeProvider } from '@assistant-ui/react';
import type { AttachmentAdapter } from '@assistant-ui/react';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { HttpAgent } from '@ag-ui/client';
import type { AgentStateMutation, AgentSubscriber, Message } from '@ag-ui/client';
import { PendingContext, type HydratedPendingApproval } from './PendingContext';
import { UsageContext, type UsageData } from './UsageContext';
import { uploadAttachment } from './api';
import { RuntimeSessionSubscriber } from './RuntimeSessionSubscriber';
import { handleHarnxCustomEvent, NAVIGATION_CONTROL_EVENTS, parseHistoryReasoning, type HistoryReasoningMap, type MessageAttachmentMeta, type CompactionOutcome, type PendingInteraction } from './harnxCustomEvents';
import { SubAgentNotesContext } from './SubAgentNotesContext';
import { INITIAL_SUB_AGENT_NOTES_STATE, reduceSubAgentNotes } from './subAgentNotes';
import { SessionNavigationContext } from './SessionNavigationContext';
import { MessageAttachmentsContext } from './MessageAttachmentsContext';
import { reduceMessageAttachments } from './messageAttachments';
import { reduceToolUpdates, type ToolUpdatesState, type ToolCallUpdatePatch } from './toolUpdates';
import { A2uiSurfacesContext } from './a2ui/A2uiSurfacesContext';
import {
  reduceSurfaces,
  type A2uiSurfacesState,
  type A2uiSurfaceEvent,
  type A2uiSurfaceResolution,
} from './a2ui/surfaceState';
import { isAbortError, observedFetch } from './httpClient';
import { connection } from './connection';
import { formatUnchangedReason } from './compactionApi';
import { HistoryReasoningSupportContext } from './historyReasoningSupport';

export interface ChatProviderProps {
  agentName: string;
  sessionId: string;
  isFreshSession: boolean;
  onHandoff?: (agent: string, sessionId: string) => void;
  onOpenSubAgent: (agent: string, sessionId: string) => void;
  onNavigateSession?: (agent: string, sessionId: string) => void;
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

// eslint-disable-next-line react-refresh/only-export-components
export function augmentSnapshotMessagesWithReasoning(
  messages: readonly unknown[],
  reasoningMap: HistoryReasoningMap,
): unknown[] {
  if (Object.keys(reasoningMap).length === 0) {
    return [...messages];
  }
  const nextMessages: unknown[] = [];
  for (const rawMessage of messages) {
    if (
      rawMessage !== null &&
      typeof rawMessage === 'object' &&
      (rawMessage as Record<string, unknown>).role === 'assistant'
    ) {
      const msgId = (rawMessage as Record<string, unknown>).id;
      if (typeof msgId === 'string' && msgId.trim().length > 0 && Object.hasOwn(reasoningMap, msgId)) {
        const thoughts = reasoningMap[msgId];
        if (Array.isArray(thoughts)) {
          thoughts.forEach((thought, idx) => {
            if (typeof thought === 'string' && thought.trim().length > 0) {
              nextMessages.push({
                id: `${msgId}:reasoning-${idx}`,
                role: 'reasoning',
                content: thought,
              });
            }
          });
        }
      }
    }
    nextMessages.push(rawMessage);
  }
  return nextMessages;
}

function augmentSnapshotPayloadWithReasoning(
  payload: AgUiSnapshotPayload,
  reasoningMap: HistoryReasoningMap,
): AgUiSnapshotPayload {
  const messages = payload?.event?.messages;
  if (!Array.isArray(messages) || Object.keys(reasoningMap).length === 0) {
    return payload;
  }
  const nextMessages = augmentSnapshotMessagesWithReasoning(messages, reasoningMap);
  if (nextMessages.length === messages.length) {
    return payload;
  }
  return {
    ...payload,
    event: {
      ...payload.event,
      messages: nextMessages as Message[],
    },
  };
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
  onPendingInteraction?: (interaction: PendingInteraction | null) => void;
  onA2uiSurface?: (surface: A2uiSurfaceEvent) => void;
  onMessageAttachments?: (messageId: string, attachments: MessageAttachmentMeta[]) => void;
  onCompactingStarted?: (compactionId?: string) => void;
  onCompactingCompleted?: (outcome: CompactionOutcome, compactionId?: string) => void;
  onCompactingFailed?: (error: string, compactionId?: string) => void;
  onSessionCompaction?: (compaction: CompactionMetadata | null) => void;
  onToolUpdate?: (patch: import('./toolUpdates').ToolCallUpdatePatch) => void;
  onHistoryReasoning?: (reasoning: HistoryReasoningMap) => void;
  onRunTerminal?: () => void;
  onTurnInterrupted?: () => void;
  isForeground?: boolean;
}

type AgUiSnapshotPayload = Parameters<NonNullable<AgentSubscriber['onMessagesSnapshotEvent']>>[0];

function isSameOriginUrl(url: string): boolean {
  if (typeof window === 'undefined' || !window.location?.origin) {
    return url.startsWith('/') && !url.startsWith('//');
  }
  try {
    return new URL(url, window.location.href).origin === window.location.origin;
  } catch {
    return false;
  }
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

const SNAPSHOT_REASONING_FLUSH_MS = 50;

// eslint-disable-next-line react-refresh/only-export-components
export class HarnxHttpAgent extends HttpAgent {
  private readonly onStatus: (text: string | null) => void;
  private readonly onRunFailedCb: (message: string) => void;
  private readonly onUsageCb: (usage: UsageData) => void;
  private readonly onToolSummaryCb: (id: string, summary: string) => void;
  private readonly onHandoff?: (agent: string, sessionId: string) => void;
  private readonly onSubAgentEvent: (event: unknown) => void;
  private readonly onHitlPendingApproval?: (toolCallId: string, summary: string) => void;
  private readonly onPendingInteractionCb?: (interaction: PendingInteraction | null) => void;
  private readonly onA2uiSurfaceCb?: (surface: A2uiSurfaceEvent) => void;
  private readonly onMessageAttachments?: (messageId: string, attachments: MessageAttachmentMeta[]) => void;
  private readonly onCompactingStarted?: (compactionId?: string) => void;
  private readonly onCompactingCompleted?: (outcome: CompactionOutcome, compactionId?: string) => void;
  private readonly onCompactingFailed?: (error: string, compactionId?: string) => void;
  private readonly onSessionCompaction?: (compaction: CompactionMetadata | null) => void;
  private readonly onToolUpdate?: (patch: import('./toolUpdates').ToolCallUpdatePatch) => void;
  private readonly onHistoryReasoning?: (reasoning: HistoryReasoningMap) => void;
  private readonly onRunTerminalCb?: () => void;
  private readonly onTurnInterruptedCb?: () => void;
  private readonly isForeground: boolean;
  private handoffBoundarySeq?: number;

  constructor(options: HarnxHttpAgentOptions) {
    super({
      url: options.url,
      fetch: observedFetch,
      ...(isSameOriginUrl(options.url)
        ? {
            headers: {
              'x-harnx-snapshot-features': 'history-reasoning',
            },
          }
        : {}),
    });
    this.onStatus = options.onStatus;
    this.onRunFailedCb = options.onRunFailed;
    this.onUsageCb = options.onUsage;
    this.onToolSummaryCb = options.onToolSummary;
    this.onHandoff = options.onHandoff;
    this.onSubAgentEvent = options.onSubAgentEvent;
    this.onHitlPendingApproval = options.onHitlPendingApproval;
    this.onPendingInteractionCb = options.onPendingInteraction;
    this.onA2uiSurfaceCb = options.onA2uiSurface;
    this.onMessageAttachments = options.onMessageAttachments;
    this.onCompactingStarted = options.onCompactingStarted;
    this.onCompactingCompleted = options.onCompactingCompleted;
    this.onCompactingFailed = options.onCompactingFailed;
    this.onSessionCompaction = options.onSessionCompaction;
    this.onToolUpdate = options.onToolUpdate;
    this.onHistoryReasoning = options.onHistoryReasoning;
    this.onRunTerminalCb = options.onRunTerminal;
    this.onTurnInterruptedCb = options.onTurnInterrupted;
    this.isForeground = options.isForeground !== false;
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
      onPendingInteraction: this.onPendingInteractionCb,
      onA2uiSurface: this.onA2uiSurfaceCb,
      onMessageAttachments: this.onMessageAttachments,
      onCompactingStarted: this.onCompactingStarted,
      onCompactingCompleted: this.onCompactingCompleted,
      onCompactingFailed: this.onCompactingFailed,
      onSessionCompaction: this.onSessionCompaction,
      onToolUpdate: this.onToolUpdate,
      onHistoryReasoning: this.onHistoryReasoning,
      onTurnInterrupted: () => {
        this.onTurnInterruptedCb?.();
        this.onRunTerminalCb?.();
      },
      isForeground: this.isForeground,
    });
  }

  private handleTerminalEvent(event: any) {
    this.onRunTerminalCb?.();
    if (event?.type === 'RUN_ERROR') {
      // AG-UI delivers server failures as events; onRunFailed only handles
      // failures of the client stream. Preserve the server's diagnostic text.
      this.onRunFailedCb(event.message || 'Failed to send message');
    }
  }

  private handleAgentEvent(event: any) {
    this.onSubAgentEvent(event);
    if (event?.type === 'CUSTOM') {
      this.handleCustomEvent(event.name, event.value);
    } else if (event?.type === 'RUN_FINISHED' || event?.type === 'RUN_ERROR') {
      this.handleTerminalEvent(event);
    }
  }

  private handleRunFailure(message: string, error?: unknown) {
    if (isBenignRunAbort(error, message)) {
      return;
    }
    this.onSubAgentEvent({ type: 'RUN_ERROR' });
    this.onRunTerminalCb?.();
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

  private wrapSubscriber(subscriber?: AgentSubscriber): {
    subscriber: AgentSubscriber;
    cleanup: () => void;
  } {
    let pendingSnapshotPayload: AgUiSnapshotPayload | null = null;
    let lastFlushedIdleSnapshot: AgUiSnapshotPayload | null = null;
    let pendingIdleMessagesSync: Message[] | undefined;
    let snapshotReasoning: HistoryReasoningMap = {};
    let fallbackFlushTimer: ReturnType<typeof setTimeout> | undefined;
    let runEnded = false;

    const clearFallbackTimer = () => {
      if (fallbackFlushTimer !== undefined) {
        clearTimeout(fallbackFlushTimer);
        fallbackFlushTimer = undefined;
      }
    };

    const syncAgentMessages = (messages: Message[] | undefined) => {
      if (messages !== undefined) {
        this.setMessages(messages);
      }
    };

    const dispatchSnapshot = async (
      payload: AgUiSnapshotPayload,
      reasoning: HistoryReasoningMap,
    ): Promise<Message[] | undefined> => {
      const nextPayload = augmentSnapshotPayloadWithReasoning(payload, reasoning);
      const nextMessages = Array.isArray(nextPayload?.event?.messages)
        ? nextPayload.event.messages
        : undefined;
      try {
        await subscriber?.onMessagesSnapshotEvent?.(nextPayload);
      } catch (err) {
        console.error('[HarnxHttpAgent] Failed to deliver snapshot event:', err);
      }
      return nextMessages;
    };

    const flushPendingSnapshot = async (
      keepForLateReasoning = false,
    ): Promise<Message[] | undefined> => {
      clearFallbackTimer();
      if (!pendingSnapshotPayload) return undefined;
      const payload = pendingSnapshotPayload;
      pendingSnapshotPayload = null;
      const reasoning = snapshotReasoning;
      snapshotReasoning = {};
      lastFlushedIdleSnapshot = keepForLateReasoning ? payload : null;
      return await dispatchSnapshot(payload, reasoning);
    };

    const consumeFlushedMessages = async (): Promise<Message[] | undefined> => {
      lastFlushedIdleSnapshot = null;
      const flushedMessages = await flushPendingSnapshot(false);
      if (flushedMessages !== undefined) {
        pendingIdleMessagesSync = undefined;
        return flushedMessages;
      }
      const idleSync = pendingIdleMessagesSync;
      pendingIdleMessagesSync = undefined;
      return idleSync;
    };

    const withFlushedMessages = <T extends AgentStateMutation | void>(
      result: T,
      flushedMessages: Message[] | undefined,
    ): T | { messages: Message[] } => {
      if (flushedMessages === undefined) return result;
      if (result && typeof result === 'object' && 'messages' in result) {
        return result;
      }
      return result && typeof result === 'object'
        ? { ...result, messages: flushedMessages }
        : { messages: flushedMessages };
    };

    const finishRun = async <TPayload, TResult extends AgentStateMutation | void>(
      downstream?: (payload: TPayload) => TResult | Promise<TResult>,
      payload?: TPayload,
    ): Promise<TResult | { messages: Message[] }> => {
      runEnded = true;
      const flushedMessages = await consumeFlushedMessages();
      const result = payload !== undefined ? await downstream?.(payload) : undefined;
      return withFlushedMessages(result as TResult, flushedMessages);
    };

    return {
      cleanup: () => {
        runEnded = true;
        clearFallbackTimer();
        pendingIdleMessagesSync = undefined;
      },
      subscriber: {
        ...subscriber,
        onEvent: async (payload) => {
          const event = payload?.event as { type?: unknown; name?: unknown } | undefined;
          const isHistoryReasoning =
            event?.type === 'CUSTOM' && event?.name === 'history_reasoning';
          let flushedMessages: Message[] | undefined;
          if (event?.type !== 'MESSAGES_SNAPSHOT' && !isHistoryReasoning) {
            flushedMessages = await consumeFlushedMessages();
          }
          this.handleAgentEvent(event);
          const result = await subscriber?.onEvent?.(payload);
          return withFlushedMessages(result, flushedMessages);
        },
        onMessagesSnapshotEvent: async (payload) => {
          pendingIdleMessagesSync = undefined;
          await consumeFlushedMessages();
          snapshotReasoning = {};
          pendingSnapshotPayload = payload;
          if (runEnded) {
            const flushedMessages = await flushPendingSnapshot(false);
            return withFlushedMessages(undefined, flushedMessages);
          }
          fallbackFlushTimer = setTimeout(() => {
            fallbackFlushTimer = undefined;
            if (!runEnded && pendingSnapshotPayload) {
              void flushPendingSnapshot(true)
                .then((flushedMessages) => {
                  pendingIdleMessagesSync = flushedMessages;
                  syncAgentMessages(flushedMessages);
                })
                .catch((err) => {
                  console.error('[HarnxHttpAgent] Failed in fallback snapshot flush:', err);
                });
            }
          }, SNAPSHOT_REASONING_FLUSH_MS);
        },
        onCustomEvent: async (payload) => {
          const event = payload?.event as { name?: unknown; value?: unknown } | undefined;
          if (event?.name === 'history_reasoning') {
            const parsed = parseHistoryReasoning(event?.value) ?? {};
            const idleSync = pendingIdleMessagesSync;
            pendingIdleMessagesSync = undefined;
            if (pendingSnapshotPayload) {
              snapshotReasoning = parsed;
              const flushedMessages = await flushPendingSnapshot(false);
              return withFlushedMessages(undefined, flushedMessages);
            }
            if (lastFlushedIdleSnapshot) {
              const idlePayload = lastFlushedIdleSnapshot;
              lastFlushedIdleSnapshot = null;
              if (Object.keys(parsed).length > 0) {
                const nextMessages = await dispatchSnapshot(idlePayload, parsed);
                return withFlushedMessages(undefined, nextMessages);
              }
              return withFlushedMessages(undefined, idleSync);
            }
            return;
          }
          const flushedMessages = await consumeFlushedMessages();
          if (!this.forwardsCustomEvent(event?.name)) {
            return withFlushedMessages(undefined, flushedMessages);
          }
          const result = await subscriber?.onCustomEvent?.(payload);
          return withFlushedMessages(result, flushedMessages);
        },
        onRunFinishedEvent: async (payload) => finishRun(subscriber?.onRunFinishedEvent, payload),
        onRunErrorEvent: async (payload) => finishRun(subscriber?.onRunErrorEvent, payload),
        onRunFinalized: async (payload) => finishRun(subscriber?.onRunFinalized, payload),
        onRunFailed: async (payload) => {
          runEnded = true;
          const flushedMessages = await consumeFlushedMessages();
          syncAgentMessages(flushedMessages);
          this.handleRunFailure(payload.error.message, payload.error);
          return subscriber?.onRunFailed?.(payload);
        },
      },
    };
  }

  override async runAgent(params: any, subscriber?: AgentSubscriber) {
    const { subscriber: wrappedSubscriber, cleanup } = this.wrapSubscriber(subscriber);

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
    } finally {
      cleanup();
    }
  }
}

export const ChatProvider: React.FC<ChatProviderProps> = ({
  agentName,
  sessionId,
  isFreshSession,
  onHandoff,
  onOpenSubAgent,
  onNavigateSession,
  onReadUpdated,
  children,
}) => {
  const cancellation = useCancellation(agentName, sessionId);
  const cancellationRef = useRef(cancellation);
  useEffect(() => {
    cancellationRef.current = cancellation;
  });
  const [statusText, setStatusText] = useState<string | null>(null);
  const [errorText, setErrorText] = useState<string | null>(null);
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const [usage, setUsage] = useState<UsageData | null>(null);
  const [toolSummaries, setToolSummaries] = useState<Map<string, string>>(new Map());
  const [compactionMetadata, setCompactionMetadata] = useState<CompactionMetadata | null>(null);
  const [compactionPhase, setCompactionPhase] = useState<{ phase: CompactionPhase; compactionId?: string }>({ phase: 'idle' });
  const [subAgentState, dispatchSubAgentEvent] = useReducer(
    reduceSubAgentNotes,
    INITIAL_SUB_AGENT_NOTES_STATE,
  );
  const [attachmentsState, dispatchAttachments] = useReducer(
    reduceMessageAttachments,
    {
      agent: agentName,
      session: sessionId,
      attachmentsByMessageId: {},
    },
  );
  const [toolUpdatesState, dispatchToolUpdate] = useReducer(
    reduceToolUpdates,
    new Map() as ToolUpdatesState,
  );
  const [surfacesState, dispatchSurfaces] = useReducer(
    reduceSurfaces,
    new Map() as A2uiSurfacesState,
  );
  // Hydrated HITL pending approvals from durable log (hitl_pending_approval CUSTOM events)
  const [hydratedApprovals, setHydratedApprovals] = useState<HydratedPendingApproval[]>([]);
  const [pendingInteraction, setPendingInteraction] = useState<PendingInteraction | null>(null);
  const [hasHistoryReasoning, setHasHistoryReasoning] = useState(false);

  const clearPendingInteraction = useCallback(() => {
    setPendingInteraction(null);
  }, []);

  const clearPendingInteractionIfMatch = useCallback(
    (interactionId: string, requestSeq: number) => {
      setPendingInteraction((prev) => {
        if (prev && prev.interactionId === interactionId && prev.requestSeq === requestSeq) {
          return null;
        }
        return prev;
      });
    },
    [],
  );
  
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

  const currentSessionRef = useRef({ agentName, sessionId });
  currentSessionRef.current = { agentName, sessionId };
  const currentAgentRef = useRef<HarnxHttpAgent | null>(null);

  const agent = useMemo(() => {
    const boundAgentName = agentName;
    const boundSessionId = sessionId;
    const agentInstance: HarnxHttpAgent = new HarnxHttpAgent({
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
      isForeground: true,
      onHitlPendingApproval: (toolCallId, summary) => {
        cancellationRef.current?.reset?.();
        addHydratedApproval({ toolCallId, summary });
      },
      onPendingInteraction: (interaction) => {
        if (
          currentAgentRef.current !== agentInstance ||
          currentSessionRef.current.agentName !== boundAgentName ||
          currentSessionRef.current.sessionId !== boundSessionId
        ) {
          return;
        }
        cancellationRef.current?.reset?.();
        setPendingInteraction(interaction);
      },
      onA2uiSurface: (surface) => {
        if (
          currentAgentRef.current !== agentInstance ||
          currentSessionRef.current.agentName !== boundAgentName ||
          currentSessionRef.current.sessionId !== boundSessionId
        ) {
          return;
        }
        dispatchSurfaces({ type: 'SURFACE_EVENT', event: surface });
      },
      onRunTerminal: () => {
        cancellationRef.current?.reset?.();
        setCompactionPhase(compactionAfterRunTerminal);
      },
      onMessageAttachments: (messageId, attachments) => {
        dispatchAttachments({
          type: 'SET_ATTACHMENTS',
          agent: agentName,
          session: sessionId,
          messageId,
          attachments,
        });
      },
      onSubAgentEvent: (event: any) => dispatchSubAgentEvent(event),
      onCompactingStarted: (compactionId) => {
        setCompactionPhase({ phase: 'compacting', compactionId });
      },
      onCompactingCompleted: (outcome, _compactionId) => {
        setCompactionPhase({ phase: 'idle' });
        if (outcome.status === 'unchanged') {
          setStatusMessage(formatUnchangedReason(outcome.detail));
        }
      },
      onCompactingFailed: (error, _compactionId) => {
        setCompactionPhase({ phase: 'failed' });
        setErrorText(error);
      },
      onSessionCompaction: (metadata) => {
        setCompactionMetadata(metadata);
      },
      onToolUpdate: (patch: ToolCallUpdatePatch) => {
        dispatchToolUpdate({ type: 'TOOL_UPDATE', patch });
      },
      onHistoryReasoning: () => {
        if (
          currentAgentRef.current !== agentInstance ||
          currentSessionRef.current.agentName !== boundAgentName ||
          currentSessionRef.current.sessionId !== boundSessionId
        ) {
          return;
        }
        setHasHistoryReasoning(true);
      },
    });
    currentAgentRef.current = agentInstance;
    return agentInstance;
  }, [agentName, sessionId, onHandoff, addHydratedApproval]);

  useEffect(() => {
    currentAgentRef.current = agent;
    return () => {
      if (currentAgentRef.current === agent) {
        currentAgentRef.current = null;
      }
    };
  }, [agent]);

  const runtime = useAgUiRuntime({
    agent,
    adapters: {
      attachments,
    }
  });

  useEffect(() => {
    setStatusText(null);
    setErrorText(null);
    setStatusMessage(null);
    setCompactionMetadata(null);
    setCompactionPhase({ phase: 'idle' });
    dispatchSubAgentEvent({ type: 'RESET' });
    dispatchAttachments({ type: 'RESET', agent: agentName, session: sessionId });
    dispatchToolUpdate({ type: 'RESET' });
    dispatchSurfaces({ type: 'RESET' });
    clearHydratedApprovals();
    clearPendingInteraction();
    setHasHistoryReasoning(false);
  }, [agentName, sessionId, clearHydratedApprovals, clearPendingInteraction]);

  const subAgentContext = useMemo(() => ({
    notes: subAgentState.notes,
    openSession: onOpenSubAgent,
    dispatch: dispatchSubAgentEvent,
  }), [onOpenSubAgent, subAgentState.notes, dispatchSubAgentEvent]);

  const sessionNavigationContext = useMemo(() => ({
    navigateSession: onNavigateSession ?? onOpenSubAgent,
  }), [onNavigateSession, onOpenSubAgent]);

  const messageAttachmentsContext = useMemo(() => ({
    agent: agentName,
    session: sessionId,
    attachmentsByMessageId: attachmentsState.attachmentsByMessageId,
  }), [agentName, sessionId, attachmentsState.attachmentsByMessageId]);

  const compactionMetadataContextValue = useMemo(() => ({
    metadata: compactionMetadata,
    agent: agentName,
    session: sessionId,
  }), [compactionMetadata, agentName, sessionId]);

  const a2uiSurfacesContextValue = useMemo(() => {
    const allSurfaces = Array.from(surfacesState.values());
    return {
      agentName,
      sessionId,
      surfaces: surfacesState,
      getSurfacesForToolCall: (toolCallId: string) =>
        allSurfaces.filter((s) => s.toolCallId === toolCallId),
      unattachedSurfaces: allSurfaces.filter((s) => !s.toolCallId),
      updateSurfaceResolution: (
        surfaceId: string,
        publishSeq: number,
        resolution: A2uiSurfaceResolution
      ) => {
        dispatchSurfaces({
          type: 'SURFACE_RESOLVED',
          surfaceId,
          publishSeq,
          resolution,
        });
      },
    };
  }, [agentName, sessionId, surfacesState]);

  return (
    <HistoryReasoningSupportContext.Provider value={hasHistoryReasoning}>
    <CancellationContext.Provider value={cancellation}>
    <CompactionContext.Provider value={compactionPhase}>
    <CompactionMetadataContext.Provider value={compactionMetadataContextValue}>
    <SessionNavigationContext.Provider value={sessionNavigationContext}>
    <SubAgentNotesContext.Provider value={subAgentContext}>
    <MessageAttachmentsContext.Provider value={messageAttachmentsContext}>
    <A2uiSurfacesContext.Provider value={a2uiSurfacesContextValue}>
      <PendingContext.Provider value={{ 
        statusText, setStatusText, errorText, setErrorText,
        statusMessage, setStatusMessage,
        hydratedApprovals, addHydratedApproval, clearHydratedApprovals, removeHydratedApproval,
        pendingInteraction, setPendingInteraction, clearPendingInteraction, clearPendingInteractionIfMatch
      }}>
        <UsageContext.Provider value={{ usage, toolSummaries, toolUpdates: toolUpdatesState }}>
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
    </A2uiSurfacesContext.Provider>
    </MessageAttachmentsContext.Provider>
    </SubAgentNotesContext.Provider>
    </SessionNavigationContext.Provider>
    </CompactionMetadataContext.Provider>
    </CompactionContext.Provider>
    </CancellationContext.Provider>
    </HistoryReasoningSupportContext.Provider>
  );
};
