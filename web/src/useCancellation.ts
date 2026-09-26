import { useCallback, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { cancel } from './api';
import { isAbortError } from './httpClient';
import type { CancellationControl } from './CancellationContext';

type Phase = CancellationControl['phase'];

export function useCancellation(agent: string, session: string): CancellationControl {
  const target = `${agent}\0${session}`;
  const [state, setState] = useState<{ target: string; phase: Phase }>({ target, phase: 'idle' });
  const phase = state.target === target ? state.phase : 'idle';
  const setPhase = useCallback((phase: Phase) => {
    setState(previous => previous.target === target && previous.phase === phase ? previous : { target, phase });
  }, [target]);

  const currentTarget = useRef(target);
  useLayoutEffect(() => { currentTarget.current = target; }, [target]);

  const reset = useCallback(() => {
    setState(previous => previous.target === target && previous.phase === 'requesting'
      ? { target, phase: 'idle' }
      : previous);
  }, [target]);

  const stop = useCallback(async () => {
    setPhase('requesting');
    try {
      // The interrupt is durable the moment it is answered, whichever outcome
      // it carries: there is nothing left to wait for, so the composer returns.
      await cancel(agent, session);
      if (currentTarget.current === target) setPhase('idle');
    } catch (err) {
      // An aborted or timed-out request says nothing about the append; leave
      // the phase alone and let event terminal or reset settle it.
      if (isAbortError(err)) return;
      if (currentTarget.current === target) setPhase('failed');
    }
  }, [agent, session, setPhase, target]);

  return useMemo(() => ({ phase, stop, reset }), [phase, stop, reset]);
}
