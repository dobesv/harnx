import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { cancel, sessionControl } from './api';
import { isAbortError } from './httpClient';
import type { CancellationControl } from './CancellationContext';

type Phase = CancellationControl['phase'];

// How often the composer re-reads the session's own view of its interrupt.
const HYDRATE_INTERVAL_MS = 500;

export function useCancellation(agent: string, session: string): CancellationControl {
  const target = `${agent}\0${session}`;
  const [state, setState] = useState<{ target: string; phase: Phase }>({ target, phase: 'idle' });
  const phase = state.target === target ? state.phase : 'idle';
  const setPhase = useCallback((phase: Phase) => {
    setState(previous => previous.target === target && previous.phase === phase ? previous : { target, phase });
  }, [target]);
  // A failed interrupt is the one phase the server cannot talk us out of: the
  // session is still running, which is exactly why the retry must stay offered.
  const hydratePhase = useCallback((phase: Phase) => {
    setState(previous => previous.target === target && (previous.phase === 'failed' || previous.phase === phase)
      ? previous
      : { target, phase });
  }, [target]);
  const requestPending = useRef(false);
  const requestVersion = useRef(0);
  const currentTarget = useRef(target);
  useLayoutEffect(() => { currentTarget.current = target; }, [target]);

  const stop = useCallback(async () => {
    setPhase('requesting');
    requestPending.current = true;
    const version = ++requestVersion.current;
    try {
      // The interrupt is durable the moment it is answered, whichever outcome
      // it carries: there is nothing left to wait for, so the composer returns.
      await cancel(agent, session);
      if (currentTarget.current === target) setPhase('idle');
    } catch (err) {
      // An aborted or timed-out request says nothing about the append; leave
      // the phase alone and let hydration settle it.
      if (isAbortError(err)) return;
      if (currentTarget.current === target) setPhase('failed');
    } finally {
      if (requestVersion.current === version) requestPending.current = false;
    }
  }, [agent, session, setPhase, target]);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    requestPending.current = false;
    ++requestVersion.current;
    const hydrate = async () => {
      const version = requestVersion.current;
      try {
        const status = await sessionControl(agent, session, { signal: controller.signal });
        if (disposed) return;
        if (!requestPending.current && version === requestVersion.current) {
          hydratePhase(status.state.status === 'interrupting' ? 'requesting' : 'idle');
        }
      } catch {
        // Losing contact with the server says nothing about this session's
        // interrupt; the next poll answers.
      }
      if (!disposed) timer = setTimeout(hydrate, HYDRATE_INTERVAL_MS);
    };
    void hydrate();
    return () => {
      disposed = true;
      clearTimeout(timer);
      controller.abort();
    };
  }, [agent, session, hydratePhase]);
  return { phase, stop };
}
