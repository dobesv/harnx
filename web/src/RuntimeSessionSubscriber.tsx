import { useEffect, useRef } from 'react';
import { useAui } from '@assistant-ui/react';

export const RuntimeSessionSubscriber = ({
  enabled,
  eventsUrl,
  onReadUpdated,
}: {
  enabled: boolean;
  eventsUrl: string;
  onReadUpdated?: () => void;
}) => {
  const aui = useAui();
  const auiRef = useRef(aui);
  auiRef.current = aui;
  const pendingRef = useRef(false);
  const refreshingRef = useRef(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onReadUpdatedRef = useRef(onReadUpdated);
  onReadUpdatedRef.current = onReadUpdated;

  useEffect(() => {
    if (!enabled) return;
    let disposed = false;

    // SSE reconnect must refetch session list (via onReadUpdated callback)
    // This handles the reconnect re-snapshot requirement for client cache reconciliation
    const handleSseOpen = () => {
      onReadUpdatedRef.current?.();
    };

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

    if (typeof EventSource !== 'undefined') {
      const events = new EventSource(eventsUrl);
      
      // Reconnect re-snapshot: call onReadUpdated on SSE open
      events.addEventListener('session-updated', scheduleRefresh);
      events.addEventListener('read-updated', () => {
        onReadUpdatedRef.current?.();
      });
      
      // Handle SSE open (reconnect) - trigger list refresh
      events.onopen = handleSseOpen;
      
      return () => {
        disposed = true;
        events.close();
        if (timerRef.current !== null) {
          clearTimeout(timerRef.current);
          timerRef.current = null;
        }
      };
    } else {
      return () => {
        disposed = true;
        if (timerRef.current !== null) {
          clearTimeout(timerRef.current);
          timerRef.current = null;
        }
      };
    }
  }, [enabled, eventsUrl]);

  return null;
};
