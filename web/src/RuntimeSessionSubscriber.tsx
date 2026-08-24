import { useEffect, useRef } from 'react';
import { useAui } from '@assistant-ui/react';

export const RuntimeSessionSubscriber = ({
  enabled,
  eventsUrl,
}: {
  enabled: boolean;
  eventsUrl: string;
}) => {
  const aui = useAui();
  const auiRef = useRef(aui);
  auiRef.current = aui;
  const pendingRef = useRef(false);
  const refreshingRef = useRef(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (!enabled) return;
    let disposed = false;

    const scheduleRefresh = () => {
      pendingRef.current = true;
      if (timerRef.current !== null) return;
      timerRef.current = setTimeout(async () => {
        timerRef.current = null;
        if (disposed || !pendingRef.current) return;
        if (refreshingRef.current || auiRef.current.thread.getState().isRunning) {
          scheduleRefresh();
          return;
        }

        pendingRef.current = false;
        refreshingRef.current = true;
        try {
          await auiRef.current.thread.startRun({
            parentId: auiRef.current.thread.getState().messages.at(-1)?.id ?? null,
          });
        } catch (error) {
          // HarnxHttpAgent already surfaces the failure in the chat error UI.
          console.error('Failed to refresh session', error);
        } finally {
          refreshingRef.current = false;
          if (pendingRef.current) scheduleRefresh();
        }
      }, 250);
    };

    // Schedule on every effect setup. React StrictMode immediately cleans up
    // its first development setup (cancelling this timer) and then performs the
    // real setup, which must re-arm hydration rather than inherit a stale guard.
    scheduleRefresh();

    const events = new EventSource(eventsUrl);
    events.addEventListener('session-updated', scheduleRefresh);
    return () => {
      disposed = true;
      events.close();
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [enabled, eventsUrl]);

  return null;
};
