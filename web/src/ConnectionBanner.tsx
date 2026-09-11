import React from 'react';
import { useConnectionStatus, useRetryCountdownSeconds } from './useConnectionStatus';

export interface ConnectionBannerProps {
  /**
   * Hide banner if caller wants to suppress it (e.g. on initial AgentPicker
   * where blocking connecting state is already shown).
   */
  suppress?: boolean;
}

export const ConnectionBanner: React.FC<ConnectionBannerProps> = ({ suppress }) => {
  const { status, nextRetryAt } = useConnectionStatus();
  const countdown = useRetryCountdownSeconds(nextRetryAt);

  if (suppress) {
    return null;
  }

  if (status !== 'reconnecting' && status !== 'degraded') {
    return null;
  }

  const message =
    countdown != null
      ? `Having trouble reaching the server. Retrying in ${countdown}s…`
      : 'Having trouble reaching the server.';

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
