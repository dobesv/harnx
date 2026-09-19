import { useContext, useState } from 'react';
import { MessageAttachmentsContext } from './MessageAttachmentsContext';
import { getSessionAttachmentUrl } from './api';

export interface MessageAttachmentsProps {
  messageId: string;
}

export const MessageAttachments = ({ messageId }: MessageAttachmentsProps) => {
  const { agent, session, attachmentsByMessageId } = useContext(MessageAttachmentsContext);
  const attachments = attachmentsByMessageId[messageId];
  const [failedParts, setFailedParts] = useState<Record<number, boolean>>({});

  if (!attachments || attachments.length === 0) {
    return null;
  }

  const sorted = [...attachments].sort((a, b) => a.partIndex - b.partIndex);

  return (
    <div
      className="aui-message-attachments"
      role="group"
      aria-label={sorted.length === 1 ? 'Message attachment' : `${sorted.length} message attachments`}
    >
      {sorted.map((att, index) => {
        // Distinguish each image for screen readers when a message has several.
        const label = sorted.length === 1 ? 'attachment' : `attachment ${index + 1} of ${sorted.length}`;
        const isFailed = Boolean(failedParts[att.partIndex]);
        if (isFailed) {
          return (
            <span
              key={`${att.partIndex}:${att.cid}`}
              className="aui-attachment aui-attachment-unavailable"
              role="status"
              aria-label={`${label} unavailable`}
            >
              <span className="aui-attachment-unavailable-icon" aria-hidden="true">⚠️</span>
              <span className="aui-attachment-name">Attachment unavailable</span>
            </span>
          );
        }

        const src = getSessionAttachmentUrl(agent, session, att.cid);
        return (
          <img
            key={`${att.partIndex}:${att.cid}`}
            src={src}
            alt={label}
            loading="lazy"
            className="aui-message-attachment-image"
            onError={() => {
              setFailedParts((prev) => ({ ...prev, [att.partIndex]: true }));
            }}
          />
        );
      })}
    </div>
  );
};
