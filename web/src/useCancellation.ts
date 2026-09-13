import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { abandonCancellation, cancel, sessionControl } from './api';
import { isAbortError } from './httpClient';
import type { CancelResult } from './types';
import type { CancellationControl } from './CancellationContext';

type MutableRef<T> = { current: T };

async function executeCancellationAction({
  action,
  target,
  fallbackPhase,
  setPhase,
  observe,
  currentTarget,
  requestPending,
  requestVersion,
}: {
  action: () => Promise<CancelResult>;
  target: string;
  fallbackPhase: CancellationControl['phase'];
  setPhase: (phase: CancellationControl['phase']) => void;
  observe: (receipt: CancelResult) => void;
  currentTarget: MutableRef<string>;
  requestPending: MutableRef<boolean>;
  requestVersion: MutableRef<number>;
}): Promise<void> {
  const version = ++requestVersion.current;
  try {
    observe(await action());
  } catch (err) {
    if (isAbortError(err)) return;
    if (currentTarget.current === target) setPhase(fallbackPhase);
  } finally {
    if (requestVersion.current === version) requestPending.current = false;
  }
}

/* oxlint-disable react/immutability -- these arguments are React refs shared by the parent hook to serialize async cancellation requests */
function useCancellationActions({
  agent,
  session,
  target,
  setPhase,
  observe,
  acceptedExecution,
  pendingSince,
  currentTarget,
  requestPending,
  requestVersion,
}: {
  agent: string;
  session: string;
  target: string;
  setPhase: (phase: CancellationControl['phase']) => void;
  observe: (receipt: CancelResult) => void;
  acceptedExecution: MutableRef<string | undefined>;
  pendingSince: MutableRef<number | null>;
  currentTarget: MutableRef<string>;
  requestPending: MutableRef<boolean>;
  requestVersion: MutableRef<number>;
}) {
  const stop = useCallback(async () => {
    setPhase('requesting');
    pendingSince.current = Date.now();
    requestPending.current = true;
    await executeCancellationAction({
      action: () => cancel(agent, session, acceptedExecution.current),
      target,
      fallbackPhase: 'failed',
      setPhase,
      observe,
      currentTarget,
      requestPending,
      requestVersion,
    });
  }, [acceptedExecution, agent, currentTarget, observe, pendingSince, requestPending, requestVersion, session, setPhase, target]);
  const resumeAnyway = useCallback(async () => {
    const expectedExecutionId = acceptedExecution.current;
    if (!expectedExecutionId) return;
    setPhase('abandoning');
    requestPending.current = true;
    await executeCancellationAction({
      action: () => abandonCancellation(agent, session, expectedExecutionId),
      target,
      fallbackPhase: 'unconfirmed',
      setPhase,
      observe,
      currentTarget,
      requestPending,
      requestVersion,
    });
  }, [acceptedExecution, agent, currentTarget, observe, requestPending, requestVersion, session, setPhase, target]);
  return { stop, resumeAnyway };
}
/* oxlint-enable react/immutability */

export function useCancellation(agent: string, session: string): CancellationControl {
  const target = `${agent}\0${session}`;
  const [state, setState] = useState<{ target: string; phase: CancellationControl['phase'] }>({ target, phase: 'idle' });
  const phase = state.target === target ? state.phase : 'idle';
  const setPhase = useCallback((phase: CancellationControl['phase']) => {
    setState(previous => previous.target === target && previous.phase === phase ? previous : { target, phase });
  }, [target]);
  const pendingSince = useRef<number | null>(null);
  const requestPending = useRef(false);
  const requestVersion = useRef(0);
  const acceptedExecution = useRef<string | undefined>(undefined);
  const currentTarget = useRef(target);
  useLayoutEffect(() => { currentTarget.current = target; }, [target]);
  const observe = useCallback((receipt: CancelResult) => {
    if (currentTarget.current !== target) return;
    acceptedExecution.current = receipt.execution_id ?? undefined;
    switch (receipt.disposition) {
      case 'idle': case 'cancelled':
        pendingSince.current = null;
        setPhase('idle');
        break;
      case 'unconfirmed': setPhase('unconfirmed'); break;
      default:
        // The server measures graph progress. Keep a separate deadline only
        // for losing contact; a progressing cascade may take longer than 5s.
        pendingSince.current = Date.now();
        setPhase('stopping');
    }
  }, [target, setPhase]);
  const { stop, resumeAnyway } = useCancellationActions({
    agent, session, target, setPhase, observe, acceptedExecution, pendingSince,
    currentTarget, requestPending, requestVersion,
  });

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    pendingSince.current = null;
    requestPending.current = false;
    acceptedExecution.current = undefined;
    ++requestVersion.current;
    const hydrate = async () => {
      const version = requestVersion.current;
      try {
        const status = await sessionControl(agent, session, { signal: controller.signal });
        if (disposed) return;
        if (!requestPending.current && version === requestVersion.current) {
          if (status.state.cancellation) observe(status.state.cancellation);
          else if (status.execution_state === 'cancelled') observe({ cancelled: true, disposition: 'cancelled' });
          else if (status.execution_state === 'completed' && status.execution_id === acceptedExecution.current) observe({ cancelled: false, disposition: 'idle' });
        }
      } catch {
        if (!disposed && pendingSince.current !== null && Date.now() - pendingSince.current >= 5000) setPhase('unconfirmed');
      }
      if (!disposed) timer = setTimeout(hydrate, 500);
    };
    void hydrate();
    return () => {
      disposed = true;
      clearTimeout(timer);
      controller.abort();
    };
  }, [agent, session, observe, setPhase]);
  return { phase, stop, resumeAnyway, observe };
}
