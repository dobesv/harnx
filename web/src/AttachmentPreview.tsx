import { useCallback, useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useHistoryDialog } from './useHistoryDialog';

const TEXT_PREVIEW_LIMIT = 256 * 1024;

function isText(file: File): boolean {
  if (file.type.startsWith('image/') || file.type === 'application/pdf') return false;
  return file.type.startsWith('text/')
    || /^application\/(?:[\w.+-]+\+)?(?:json|xml|yaml|javascript)$/i.test(file.type)
    || ((!file.type || file.type === 'application/octet-stream')
      && /\.(?:txt|md|json|ya?ml|csv|log|rs|py|js|ts|tsx|toml)$/i.test(file.name));
}

function BinaryPreview({ file, onError }: { file: File; onError: () => void }) {
  const mediaRef = useRef<HTMLImageElement | HTMLIFrameElement>(null);
  useEffect(() => {
    const media = mediaRef.current;
    if (!media) return;
    const objectUrl = URL.createObjectURL(file);
    media.addEventListener('error', onError);
    media.src = objectUrl;
    return () => {
      media.removeEventListener('error', onError);
      URL.revokeObjectURL(objectUrl);
    };
  }, [file, onError]);
  if (file.type.startsWith('image/')) return <img ref={mediaRef as React.Ref<HTMLImageElement>} className="attachment-preview-image" alt={file.name} />;
  if (file.type === 'application/pdf') return <iframe ref={mediaRef as React.Ref<HTMLIFrameElement>} tabIndex={0} className="attachment-preview-pdf" title={file.name} />;
  return <p>Preview unavailable for this file type.</p>;
}

function PreviewContent({ file }: { file: File }) {
  const [text, setText] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const onError = useCallback(() => setFailed(true), []);
  const textual = isText(file);
  useEffect(() => {
    if (textual) {
      const reader = new FileReader();
      reader.onload = () => setText(new TextDecoder().decode(reader.result as ArrayBuffer, {
        stream: file.size > TEXT_PREVIEW_LIMIT,
      }));
      reader.onerror = () => setFailed(true);
      reader.readAsArrayBuffer(file.slice(0, TEXT_PREVIEW_LIMIT));
      return () => { reader.onload = null; reader.onerror = null; reader.abort(); };
    }
  }, [file, textual]);

  if (failed) return <p role="alert">Unable to preview attachment.</p>;
  if (textual) return text === null ? <p role="status">Loading preview…</p> : <>
    <pre className="attachment-preview-text">{text}</pre>
    {file.size > TEXT_PREVIEW_LIMIT && <p>Preview truncated. The full file will be sent.</p>}
  </>;
  return <BinaryPreview file={file} onError={onError} />;
}

export function AttachmentPreview({ file, onClose, onRemove }: {
  file: File;
  onClose: () => void;
  onRemove?: () => void;
}) {
  const dialogRef = useRef<HTMLElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  useHistoryDialog(dialogRef, closeRef, onClose);

  return createPortal(
    <div className="cid-document-backdrop" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onClose();
    }}>
      <section ref={dialogRef} className="cid-document-dialog" role="dialog" aria-modal="true" aria-labelledby="attachment-preview-title">
        <header className="cid-document-header">
          <div className="cid-document-heading">
            <h2 id="attachment-preview-title">{file.name}</h2>
            <span>{file.type || 'Unknown file type'} · {file.size.toLocaleString()} bytes</span>
          </div>
          {onRemove && <button type="button" onClick={onRemove} aria-label={`Remove ${file.name}`}>Remove attachment</button>}
          <button type="button" ref={closeRef} onClick={onClose} aria-label="Close attachment preview">×</button>
        </header>
        <div className="cid-document-content"><PreviewContent key={`${file.name}:${file.lastModified}`} file={file} /></div>
      </section>
    </div>,
    document.body,
  );
}
