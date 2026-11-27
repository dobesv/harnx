import { useCallback, useState } from 'react';
import { AttachmentPrimitive, useAui, useAuiState } from '@assistant-ui/react';
import { AttachmentPreview } from './AttachmentPreview';

export function PendingAttachment() {
  const file = useAuiState((state) => state.attachment.file);
  const name = useAuiState((state) => state.attachment.name);
  const aui = useAui();
  const [viewing, setViewing] = useState(false);
  const close = useCallback(() => setViewing(false), []);
  const remove = () => {
    if (!window.confirm(`Remove ${name}?`)) return;
    close();
    aui.attachment.remove();
  };

  return <AttachmentPrimitive.Root className="aui-attachment">
    <AttachmentPrimitive.unstable_Thumb className="aui-attachment-thumb" />
    <div className="aui-attachment-info">
      <span className="aui-attachment-name"><AttachmentPrimitive.Name /></span>
    </div>
    <button type="button" className="aui-attachment-view" aria-label={`View ${name}`} disabled={!file} onClick={() => setViewing(true)}>View</button>
    <button type="button" className="aui-attachment-remove" aria-label={`Remove ${name}`} title="Remove attachment" onClick={remove}>
      ✖
    </button>
    {viewing && file && <AttachmentPreview file={file} onClose={close} onRemove={remove} />}
  </AttachmentPrimitive.Root>;
}
