import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';
import { ConnectionBanner } from '../ConnectionBanner';
import * as connectionStatusModule from '../useConnectionStatus';

describe('ConnectionBanner', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders nothing when status is online', () => {
    vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
      status: 'online',
      attempt: 0,
      nextRetryAt: null,
      retryingReads: 0,
    });
    vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(null);

    const { container } = render(<ConnectionBanner />);
    expect(container.firstChild).toBeNull();
  });

  it('renders nothing when suppress is true even if reconnecting', () => {
    vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
      status: 'reconnecting',
      attempt: 2,
      nextRetryAt: Date.now() + 5000,
      retryingReads: 1,
    });
    vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(5);

    const { container } = render(<ConnectionBanner suppress={true} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders countdown message when status is reconnecting and countdown is available', () => {
    vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
      status: 'reconnecting',
      attempt: 2,
      nextRetryAt: Date.now() + 4000,
      retryingReads: 1,
    });
    vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(4);

    render(<ConnectionBanner />);
    const banner = screen.getByTestId('connection-banner');
    expect(banner).toBeInTheDocument();
    expect(banner).toHaveAttribute('role', 'status');
    expect(banner).toHaveAttribute('aria-live', 'polite');
    expect(banner).toHaveTextContent('Having trouble reaching the server. Retrying in 4s…');
  });

  it('renders degraded message when countdown is null', () => {
    vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
      status: 'degraded',
      attempt: 0,
      nextRetryAt: null,
      retryingReads: 0,
    });
    vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(null);

    render(<ConnectionBanner />);
    const banner = screen.getByTestId('connection-banner');
    expect(banner).toBeInTheDocument();
    expect(banner).toHaveTextContent('Having trouble reaching the server.');
  });
});
