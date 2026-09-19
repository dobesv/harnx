import { createContext } from 'react';
import type { MessageAttachmentMeta } from './harnxCustomEvents';

export interface MessageAttachmentsContextValue {
  agent: string;
  session: string;
  attachmentsByMessageId: Record<string, MessageAttachmentMeta[]>;
}

export const MessageAttachmentsContext = createContext<MessageAttachmentsContextValue>({
  agent: '',
  session: '',
  attachmentsByMessageId: {},
});
