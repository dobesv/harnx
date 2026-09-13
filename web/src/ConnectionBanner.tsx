import React, { useEffect, useState } from 'react';
import { useConnectionStatus, useRetryCountdownSeconds } from './useConnectionStatus';

export interface ConnectionBannerProps {
  /**
   * Hide banner if caller wants to suppress it (e.g. on initial AgentPicker
   * where blocking connecting state is already shown).
   */
  suppress?: boolean;
  /**
   * Grace period in milliseconds before showing degraded banner.
   * Defaults to 2000ms.
   */
  degradedGraceMs?: number;
}

const DEFAULT_DEGRADED_GRACE_MS = 2000;

function computeBannerMessage(countdown: number | null): string {
  if (countdown != null) {
    return `Having trouble reaching the server. Retrying in ${countdown}s…`;
  }
  return 'Having trouble reaching the server.';
}

function getNextDegradedFlags(
  status: string,
  prevStatus: string
): { sustained: boolean; reconnected: boolean } {
  if (status !== 'degraded') {
    return { sustained: false, reconnected: false };
  }
  return { sustained: false, reconnected: prevStatus === 'reconnecting' };
}

function shouldSkipGraceTimer(
  status: string,
  reconnectedToDegraded: boolean,
  immediateGrace: boolean
): boolean {
  if (status !== 'degraded') return true;
  return reconnectedToDegraded || immediateGrace;
}

function isDegradedActive(
  status: string,
  immediateGrace: boolean,
  reconnectedToDegraded: boolean,
  sustainedDegraded: boolean
): boolean {
  if (status !== 'degraded') return false;
  return immediateGrace || reconnectedToDegraded || sustainedDegraded;
}

function isBannerVisible(
  status: string,
  suppress: boolean | undefined,
  degradedVisible: boolean
): boolean {
  if (suppress) return false;
  if (status === 'reconnecting') return true;
  return degradedVisible;
}

export const ConnectionBanner: React.FC<ConnectionBannerProps> = ({
  suppress,
  degradedGraceMs = DEFAULT_DEGRADED_GRACE_MS,
}) => {
  const { status, nextRetryAt } = useConnectionStatus();
  const countdown = useRetryCountdownSeconds(nextRetryAt);

  // Track status transitions during render to adjust state without synchronous effect updates
  const [prevStatus, setPrevStatus] = useState(status);
  const [sustainedDegraded, setSustainedDegraded] = useState(false);
  const [reconnectedToDegraded, setReconnectedToDegraded] = useState(false);

  if (status !== prevStatus) {
    setPrevStatus(status);
    const nextFlags = getNextDegradedFlags(status, prevStatus);
    setSustainedDegraded(nextFlags.sustained);
    setReconnectedToDegraded(nextFlags.reconnected);
  }

  const immediateGrace = degradedGraceMs <= 0;

  useEffect(() => {
    if (shouldSkipGraceTimer(status, reconnectedToDegraded, immediateGrace)) {
      return;
    }

    const timer = setTimeout(() => {
      setSustainedDegraded(true);
    }, degradedGraceMs);

    return () => {
      clearTimeout(timer);
    };
  }, [status, reconnectedToDegraded, degradedGraceMs, immediateGrace]);

  const degradedVisible = isDegradedActive(
    status,
    immediateGrace,
    reconnectedToDegraded,
    sustainedDegraded
  );

  if (!isBannerVisible(status, suppress, degradedVisible)) {
    return null;
  }

  const message = computeBannerMessage(countdown);

  return (
    <div
      role="status"
      aria-live="polite"
      className="aui-connection-banner"
      data-testid="connection-banner"
    >
      <span className="aui-connection-banner-dot" aria-hidden="true" />
      <span className="aui-connection-banner-text">{message}</span>
    </div>
  );
};
