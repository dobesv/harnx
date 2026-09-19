import type { MessageAttachmentMeta } from './harnxCustomEvents';

export interface MessageAttachmentsState {
  agent: string;
  session: string;
  attachmentsByMessageId: Record<string, MessageAttachmentMeta[]>;
}

export const INITIAL_MESSAGE_ATTACHMENTS_STATE: MessageAttachmentsState = {
  agent: '',
  session: '',
  attachmentsByMessageId: {},
};

export type MessageAttachmentsAction =
  | {
      type: 'SET_ATTACHMENTS';
      agent: string;
      session: string;
      messageId: string;
      attachments: MessageAttachmentMeta[];
    }
  | {
      type: 'RESET';
      agent?: string;
      session?: string;
    };

type SetAttachmentsAction = Extract<MessageAttachmentsAction, { type: 'SET_ATTACHMENTS' }>;

function isStaleEvent(state: MessageAttachmentsState, action: SetAttachmentsAction): boolean {
  return Boolean(
    state.agent &&
      state.session &&
      (action.agent !== state.agent || action.session !== state.session),
  );
}

function withReplacedMessage(
  state: MessageAttachmentsState,
  action: SetAttachmentsAction,
): MessageAttachmentsState {
  const sorted = [...action.attachments].sort((a, b) => a.partIndex - b.partIndex);
  return {
    ...state,
    agent: state.agent || action.agent,
    session: state.session || action.session,
    attachmentsByMessageId: {
      ...state.attachmentsByMessageId,
      // REPLACE (not append) the list for this messageId
      [action.messageId]: sorted,
    },
  };
}

export function reduceMessageAttachments(
  state: MessageAttachmentsState,
  action: MessageAttachmentsAction,
): MessageAttachmentsState {
  switch (action.type) {
    case 'RESET':
      return {
        agent: action.agent ?? '',
        session: action.session ?? '',
        attachmentsByMessageId: {},
      };
    case 'SET_ATTACHMENTS':
      return isStaleEvent(state, action) ? state : withReplacedMessage(state, action);
    default:
      return state;
  }
}
