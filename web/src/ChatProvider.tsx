import React, { useEffect, useMemo, useState } from 'react';
import { AssistantRuntimeProvider, useThreadRuntime } from '@assistant-ui/react';
import type { AttachmentAdapter } from '@assistant-ui/react';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { HttpAgent } from '@ag-ui/client';
import type { AgentSubscriber, Message } from '@ag-ui/client';
import { PendingContext } from './PendingContext';
import { UsageContext, type UsageData } from './UsageContext';
import { uploadAttachment } from './api';
import { setDocumentTitle } from './sessionTitle';

export interface ChatProviderProps {
  agentName: string;
  sessionId: string;
  isFreshSession: boolean;
  onHandoff?: (agent: string, sessionId: string | null) => void;
  children: React.ReactNode;
}

const EMPTY_STATE = {};

const RuntimeSessionSubscriber = ({ enabled }: { enabled: boolean }) => {
  const threadRuntime = useThreadRuntime();

  useEffect(() => {
    if (!enabled) return;
    threadRuntime.startRun({
      parentId: threadRuntime.getState().messages.at(-1)?.id ?? null,
    });
  }, [enabled, threadRuntime]);

  return null;
};

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
  onHandoff?: (agent: string, sessionId: string | null) => void;
}

// eslint-disable-next-line react-refresh/only-export-components
export class HarnxHttpAgent extends HttpAgent {
  private readonly onStatus: (text: string | null) => void;
  private readonly onRunFailedCb: (message: string) => void;
  private readonly onUsageCb: (usage: UsageData) => void;
  private readonly onToolSummaryCb: (id: string, summary: string) => void;
  private readonly onHandoff?: (agent: string, sessionId: string | null) => void;
  private isRunActive = false;

  constructor(options: HarnxHttpAgentOptions) {
    super({ url: options.url });
    this.onStatus = options.onStatus;
    this.onRunFailedCb = options.onRunFailed;
    this.onUsageCb = options.onUsage;
    this.onToolSummaryCb = options.onToolSummary;
    this.onHandoff = options.onHandoff;
  }

  private handleCustomEvent(name: string, value: any) {
    const handlers: Record<string, (v: any) => void> = {
      status: (v) => this.onStatus(v?.text || null),
      usage: (v) => this.onUsageCb(v),
      tool_summary: (v) => this.onToolSummaryCb(v?.tool_call_id, v?.markdown),
      session_title_updated: (v) => setDocumentTitle(v?.title),
      session_handoff: (v) => {
        if (this.isRunActive) {
          this.onHandoff?.(v?.agent, v?.session_id ?? null);
        }
      },
    };
    handlers[name]?.(value);
  }

  override async runAgent(params: any, subscriber?: AgentSubscriber) {
    const wrappedSubscriber: AgentSubscriber = {
      ...subscriber,
      onEvent: async (payload) => {
        const event = payload.event as any;
        if (event?.type === 'RUN_STARTED') {
          this.isRunActive = true;
        } else if (event?.type === 'RUN_FINISHED' || event?.type === 'RUN_ERROR') {
          this.isRunActive = false;
        } else if (event?.type === 'CUSTOM') {
          this.handleCustomEvent(event.name, event.value);
        }
        return subscriber?.onEvent?.(payload as any);
      },
      onCustomEvent: async (payload) => {
        const event = payload.event as any;
        this.handleCustomEvent(event?.name, event?.value);
        return subscriber?.onCustomEvent?.(payload as any);
      },
      onRunFailed: async (payload) => {
        this.onRunFailedCb(payload.error.message || 'Failed to send message');
        return subscriber?.onRunFailed?.(payload as any);
      },
    };

    const nextParams = {
      ...params,
      state: params?.state ?? EMPTY_STATE,
      messages: toAgUiMessages(params?.messages ?? []),
    };

    try {
      return await super.runAgent(nextParams, wrappedSubscriber);
    } catch (err: any) {
      this.onRunFailedCb(err.message || 'Failed to send message');
      throw err;
    }
  }
}

export const ChatProvider: React.FC<ChatProviderProps> = ({ agentName, sessionId, isFreshSession, onHandoff, children }) => {
  const [statusText, setStatusText] = useState<string | null>(null);
  const [errorText, setErrorText] = useState<string | null>(null);
  const [usage, setUsage] = useState<UsageData | null>(null);
  const [toolSummaries, setToolSummaries] = useState<Map<string, string>>(new Map());

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
  }), [agentName, sessionId, onHandoff]);

  const runtime = useAgUiRuntime({
    agent,
    adapters: {
      attachments,
    }
  });

  useEffect(() => {
    setStatusText(null);
    setErrorText(null);
  }, [agentName, sessionId]);

  return (
    <PendingContext.Provider value={{ statusText, setStatusText, errorText, setErrorText }}>
      <UsageContext.Provider value={{ usage, toolSummaries }}>
        <AssistantRuntimeProvider key={`${agentName}:${sessionId}`} runtime={runtime}>
          <RuntimeSessionSubscriber enabled={!isFreshSession} />
          {children}
        </AssistantRuntimeProvider>
      </UsageContext.Provider>
    </PendingContext.Provider>
  );
};
