import { useEffect, useState, useSyncExternalStore } from 'react';
import { connection, type ConnectionSnapshot } from './connection';

export function useConnectionStatus(): ConnectionSnapshot {
  return useSyncExternalStore(connection.subscribe, connection.getSnapshot);
}

function calculateSecondsRemaining(nextRetryAt: number | null): number | null {
  if (nextRetryAt === null) {
    return null;
  }
  return Math.max(0, Math.ceil((nextRetryAt - Date.now()) / 1000));
}

export function useRetryCountdownSeconds(nextRetryAt: number | null): number | null {
  const [secondsRemaining, setSecondsRemaining] = useState<number | null>(() =>
    calculateSecondsRemaining(nextRetryAt)
  );

  useEffect(() => {
    // Update immediately when nextRetryAt changes
    setSecondsRemaining(calculateSecondsRemaining(nextRetryAt));

    if (nextRetryAt === null) {
      return;
    }

    const intervalId = setInterval(() => {
      setSecondsRemaining(calculateSecondsRemaining(nextRetryAt));
    }, 1000);

    return () => {
      clearInterval(intervalId);
    };
  }, [nextRetryAt]);

  return secondsRemaining;
}
