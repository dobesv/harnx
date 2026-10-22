import { useCallback, useEffect, useState } from 'react';
import type { ArchivedHistoryTarget } from './ArchivedHistoryContext';

function isSameTarget(a: ArchivedHistoryTarget, b: ArchivedHistoryTarget): boolean {
  return a.agent === b.agent && a.session === b.session && a.beforeCompaction === b.beforeCompaction;
}

export function useHistoryNavigation(initialTarget: ArchivedHistoryTarget) {
  const [targetStack, setTargetStack] = useState<ArchivedHistoryTarget[]>([initialTarget]);
  const currentTarget = targetStack[targetStack.length - 1] ?? initialTarget;
  const canGoBack = targetStack.length > 1;

  useEffect(() => {
    setTargetStack((prev) => {
      const top = prev[0];
      if (top && isSameTarget(top, initialTarget)) return prev;
      return [initialTarget];
    });
  }, [initialTarget]);

  const goBack = useCallback(() => {
    if (targetStack.length <= 1) return;
    setTargetStack((prev) => prev.slice(0, -1));
  }, [targetStack.length]);

  const pushTarget = useCallback((compactionSeq: number) => {
    setTargetStack((prev) => {
      const last = prev[prev.length - 1];
      const agent = last?.agent ?? initialTarget.agent;
      const session = last?.session ?? initialTarget.session;
      return [
        ...prev,
        {
          agent,
          session,
          beforeCompaction: compactionSeq,
          title: `History before compaction ${compactionSeq}`,
        },
      ];
    });
  }, [initialTarget.agent, initialTarget.session]);

  return { currentTarget, canGoBack, goBack, pushTarget };
}
