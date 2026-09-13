import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';
import { ConnectionBanner } from '../ConnectionBanner';
import * as connectionStatusModule from '../useConnectionStatus';

function mockConnectionState({
  status,
  attempt = 0,
  nextRetryAt = null,
  retryingReads = 0,
  countdown = null,
}: {
  status: 'online' | 'reconnecting' | 'degraded' | 'unknown';
  attempt?: number;
  nextRetryAt?: number | null;
  retryingReads?: number;
  countdown?: number | null;
}) {
  const statusSpy = vi.spyOn(connectionStatusModule, 'useConnectionStatus').mockReturnValue({
    status,
    attempt,
    nextRetryAt,
    retryingReads,
  });
  const countdownSpy = vi.spyOn(connectionStatusModule, 'useRetryCountdownSeconds').mockReturnValue(countdown);
  return { statusSpy, countdownSpy };
}

describe('ConnectionBanner', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('renders nothing when status is online', () => {
    mockConnectionState({ status: 'online' });
    const { container } = render(<ConnectionBanner />);
    expect(container.firstChild).toBeNull();
  });

  it('renders nothing when suppress is true even if reconnecting', () => {
    mockConnectionState({
      status: 'reconnecting',
      attempt: 2,
      nextRetryAt: Date.now() + 5000,
      retryingReads: 1,
      countdown: 5,
    });
    const { container } = render(<ConnectionBanner suppress={true} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders countdown message when status is reconnecting and countdown is available', () => {
    mockConnectionState({
      status: 'reconnecting',
      attempt: 2,
      nextRetryAt: Date.now() + 4000,
      retryingReads: 1,
      countdown: 4,
    });
    render(<ConnectionBanner />);
    const banner = screen.getByTestId('connection-banner');
    expect(banner).toBeInTheDocument();
    expect(banner).toHaveAttribute('role', 'status');
    expect(banner).toHaveAttribute('aria-live', 'polite');
    expect(banner).toHaveTextContent('Having trouble reaching the server. Retrying in 4s…');
  });

  it('does not render degraded message immediately (debounced)', () => {
    vi.useFakeTimers();
    mockConnectionState({ status: 'degraded' });
    render(<ConnectionBanner />);
    expect(screen.queryByTestId('connection-banner')).toBeNull();
  });

  it('renders degraded message after trouble persists beyond grace period', () => {
    vi.useFakeTimers();
    mockConnectionState({ status: 'degraded' });
    render(<ConnectionBanner />);
    expect(screen.queryByTestId('connection-banner')).toBeNull();

    act(() => {
      vi.advanceTimersByTime(2000);
    });

    const banner = screen.getByTestId('connection-banner');
    expect(banner).toBeInTheDocument();
    expect(banner).toHaveTextContent('Having trouble reaching the server.');
  });

  it('does not render degraded message if trouble recovers within grace period', () => {
    vi.useFakeTimers();
    const { statusSpy } = mockConnectionState({ status: 'degraded' });

    const { rerender } = render(<ConnectionBanner />);
    expect(screen.queryByTestId('connection-banner')).toBeNull();

    act(() => {
      vi.advanceTimersByTime(1000);
    });

    statusSpy.mockReturnValue({
      status: 'online',
      attempt: 0,
      nextRetryAt: null,
      retryingReads: 0,
    });
    rerender(<ConnectionBanner />);

    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.queryByTestId('connection-banner')).toBeNull();
  });

  it('renders degraded message immediately when transitioning from reconnecting', () => {
    const { statusSpy, countdownSpy } = mockConnectionState({
      status: 'reconnecting',
      attempt: 1,
      nextRetryAt: Date.now() + 1000,
      retryingReads: 1,
      countdown: 1,
    });

    const { rerender } = render(<ConnectionBanner />);
    expect(screen.getByTestId('connection-banner')).toBeInTheDocument();

    statusSpy.mockReturnValue({
      status: 'degraded',
      attempt: 0,
      nextRetryAt: null,
      retryingReads: 0,
    });
    countdownSpy.mockReturnValue(null);
    rerender(<ConnectionBanner />);

    const banner = screen.getByTestId('connection-banner');
    expect(banner).toBeInTheDocument();
    expect(banner).toHaveTextContent('Having trouble reaching the server.');
  });

  it('renders degraded message immediately when degradedGraceMs is 0', () => {
    mockConnectionState({ status: 'degraded' });
    render(<ConnectionBanner degradedGraceMs={0} />);
    const banner = screen.getByTestId('connection-banner');
    expect(banner).toBeInTheDocument();
    expect(banner).toHaveTextContent('Having trouble reaching the server.');
  });
});
