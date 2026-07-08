import React, { useMemo, useState } from 'react';
import { AssistantRuntimeProvider } from '@assistant-ui/react';
import type { AttachmentAdapter } from '@assistant-ui/react';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { HttpAgent } from '@ag-ui/client';
import { PendingContext } from './PendingContext';
import { uploadAttachment, prompt } from './api';

export interface ChatProviderProps {
  agentName: string;
  sessionId: string;
  children: React.ReactNode;
}

export const ChatProvider: React.FC<ChatProviderProps> = ({ agentName, sessionId, children }) => {
  const [pendingText, setPendingText] = useState<string | null>(null);
  const [statusText, setStatusText] = useState<string | null>(null);
  const [errorText, setErrorText] = useState<string | null>(null);

  const attachments: AttachmentAdapter = useMemo(() => ({
    accept: 'image/png,image/jpeg,image/webp,image/gif,application/pdf,text/plain',
    async add({ file }) {
      return {
        id: crypto.randomUUID(),
        type: file.type.startsWith('image/') ? 'image' : 'file',
        file,
        name: file.name,
        contentType: file.type,
        status: 'pending',
      } as any;
    },
    async send(attachment) {
      if (!attachment.file) return attachment as any;
      try {
        const refs = await uploadAttachment(agentName, sessionId, attachment.file);
        return {
          ...attachment,
          status: 'complete',
          url: refs[0],
        } as any;
      } catch (err: any) {
        throw new Error(err.message);
      }
    },
    async remove() {}
  }), [agentName, sessionId]);

  const agent = useMemo(() => {
    const inner = new HttpAgent({ url: `/v1/agents/${encodeURIComponent(agentName)}/sessions/${encodeURIComponent(sessionId)}` });
    
    // Proxy the run method to listen for CUSTOM events
    const proxy = Object.create(inner);
    proxy.runAgent = async (params: any, subscriber: any) => {
      const originalSub = subscriber || {};

      const messages = params.messages || [];
      const isResume = params.resume && Array.isArray(params.resume) && params.resume.length > 0;
      let attachment_refs: string[] = [];
      let text = "";
      
      setErrorText(null);

      try {
        if (messages.length > 0) {
          const lastMsg = messages[messages.length - 1];
          if (lastMsg.role === "user" && !isResume) {
            if (Array.isArray(lastMsg.content)) {
               for (const part of lastMsg.content) {
                  if (part.type === "text") {
                     text += part.text + "\n";
                  } else if (part.type === "image" || part.type === "file") {
                     const val = part.source?.value || part.source?.url;
                     if (typeof val === "string" && val.startsWith("cid:")) {
                        attachment_refs.push(val);
                     }
                  }
               }
               text = text.trim();
            } else if (typeof lastMsg.content === "string") {
               text = lastMsg.content;
            }

            await prompt(agentName, sessionId, text, attachment_refs);
            params.messages = messages.slice(0, -1);
          }
        }

        if (isResume) {
          const transformedResume = params.resume.map((r: any) => ({
            interrupt_id: r.interruptId,
            status: r.status === 'resolved' ? 'approved' : 'denied',
            payload: {
               approved: r.status === 'resolved',
               reason: r.payload?.reason || null
            }
          }));
          await prompt(agentName, sessionId, "", [], transformedResume);
        }

        return inner.runAgent(params, {
          ...originalSub,
          onEvent: (payload: any) => {
            if (payload?.event?.type === 'CUSTOM' && payload.event.name === 'pending_message_consumed') {
               setPendingText(null);
            }
            if (payload?.event?.type === 'CUSTOM' && payload.event.name === 'status') {
               setStatusText(payload.event.value?.text || null);
            }
            if (originalSub.onEvent) {
               return originalSub.onEvent(payload);
            }
          },
          onCustomEvent: (payload: any) => {
            if (payload?.event?.name === 'pending_message_consumed') {
               setPendingText(null);
            }
            if (payload?.event?.name === 'status') {
               setStatusText(payload.event.value?.text || null);
            }
            if (originalSub.onCustomEvent) {
               return originalSub.onCustomEvent(payload);
            }
          }
        });
      } catch (err: any) {
        console.error("Failed to run agent", err);
        setErrorText(err.message || 'Failed to send message');
        throw err;
      }
    };
    return proxy;
  }, [agentName, sessionId]);

  const runtime = useAgUiRuntime({ agent, adapters: { attachments } });

  return (
    <PendingContext.Provider value={{ pendingText, setPendingText, statusText, setStatusText, errorText, setErrorText }}>
      <AssistantRuntimeProvider key={`${agentName}:${sessionId}`} runtime={runtime}>
        {children}
      </AssistantRuntimeProvider>
    </PendingContext.Provider>
  );
};
