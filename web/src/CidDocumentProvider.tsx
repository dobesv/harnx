import { useCallback, useMemo, useState, type PropsWithChildren } from 'react';
import {
  CidDocumentContext,
  type CidDocumentContextValue,
  type CidDocumentTarget,
} from './CidDocumentContext';
import { CidDocumentViewer } from './CidDocumentViewer';

export function CidDocumentProvider({ children }: PropsWithChildren) {
  const [activeDocument, setActiveDocument] = useState<CidDocumentTarget | null>(null);
  const context = useMemo<CidDocumentContextValue>(() => ({
    openDocument: setActiveDocument,
  }), []);
  const closeDocument = useCallback(() => setActiveDocument(null), []);

  return (
    <CidDocumentContext.Provider value={context}>
      {children}
      {activeDocument ? (
        <CidDocumentViewer
          key={activeDocument.cid}
          cid={activeDocument.cid}
          title={activeDocument.title}
          onClose={closeDocument}
        />
      ) : null}
    </CidDocumentContext.Provider>
  );
}
