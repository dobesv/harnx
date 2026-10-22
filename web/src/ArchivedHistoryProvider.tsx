import { useCallback, useMemo, useState, type PropsWithChildren } from 'react';
import {
  ArchivedHistoryContext,
  type ArchivedHistoryContextValue,
  type ArchivedHistoryTarget,
} from './ArchivedHistoryContext';
import { ArchivedHistoryViewer } from './ArchivedHistoryViewer';

export function ArchivedHistoryProvider({ children }: PropsWithChildren) {
  const [activeTarget, setActiveTarget] = useState<ArchivedHistoryTarget | null>(null);
  const context = useMemo<ArchivedHistoryContextValue>(() => ({
    openHistory: setActiveTarget,
  }), []);
  const closeHistory = useCallback(() => setActiveTarget(null), []);

  return (
    <ArchivedHistoryContext.Provider value={context}>
      {children}
      {activeTarget ? (
        <ArchivedHistoryViewer
          key={`${activeTarget.agent}:${activeTarget.session}:${activeTarget.beforeCompaction}`}
          initialTarget={activeTarget}
          onClose={closeHistory}
        />
      ) : null}
    </ArchivedHistoryContext.Provider>
  );
}
