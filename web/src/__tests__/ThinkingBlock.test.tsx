import { describe, it, expect } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/react';
import '@testing-library/jest-dom';
import { ThinkingBlock } from '../components/ThinkingBlock';

describe('ThinkingBlock component', () => {
  it('rerenders one instance through false -> true -> false streaming states', () => {
    const { container, rerender } = render(
      <ThinkingBlock text="Internal thought process" isStreaming={false} />,
    );

    const details = container.querySelector('details');
    expect(details).toBeInTheDocument();
    // Initially not streaming -> collapsed
    expect(details).not.toHaveAttribute('open');
    expect(screen.queryByText('…')).not.toBeInTheDocument();

    // Now stream starts on the SAME instance -> automatically opens with streaming indicator
    rerender(<ThinkingBlock text="Internal thought process" isStreaming={true} />);
    expect(details).toHaveAttribute('open');
    expect(screen.getByText('…')).toBeInTheDocument();

    // Stream completes on the SAME instance -> automatically collapses
    rerender(<ThinkingBlock text="Internal thought process" isStreaming={false} />);
    expect(details).not.toHaveAttribute('open');
    expect(screen.queryByText('…')).not.toBeInTheDocument();
  });

  it('respects user manual toggle mid-stream when collapsed by user', () => {
    const { container, rerender } = render(
      <ThinkingBlock text="Live streaming thought" isStreaming={true} />,
    );

    const details = container.querySelector('details')!;
    const summary = container.querySelector('summary')!;

    // Mounted streaming -> open
    expect(details).toHaveAttribute('open');

    // User manually clicks summary to collapse mid-stream
    fireEvent.click(summary);
    expect(details).not.toHaveAttribute('open');

    // Streaming continues or content updates
    rerender(<ThinkingBlock text="Live streaming thought updated" isStreaming={true} />);
    // User manual collapse must still be respected
    expect(details).not.toHaveAttribute('open');

    // Stream completes
    rerender(<ThinkingBlock text="Live streaming thought updated" isStreaming={false} />);
    expect(details).not.toHaveAttribute('open');
  });

  it('respects user manual toggle mid-stream when reopened after stream finishes', () => {
    const { container, rerender } = render(
      <ThinkingBlock text="Thought text" isStreaming={true} />,
    );

    const details = container.querySelector('details')!;
    const summary = container.querySelector('summary')!;
    expect(details).toHaveAttribute('open');

    // Stream completes -> auto collapses
    rerender(<ThinkingBlock text="Thought text" isStreaming={false} />);
    expect(details).not.toHaveAttribute('open');

    // User opens manually
    fireEvent.click(summary);
    expect(details).toHaveAttribute('open');

    // Subsequent rerenders stay open
    rerender(<ThinkingBlock text="Thought text" isStreaming={false} />);
    expect(details).toHaveAttribute('open');
  });

  it('supports keyboard navigation via Enter and Space on summary', () => {
    const { container } = render(
      <ThinkingBlock text="Keyboard accessible thought" isStreaming={false} />,
    );

    const details = container.querySelector('details')!;
    const summary = container.querySelector('summary')!;
    expect(details).not.toHaveAttribute('open');

    // Press Enter to expand
    fireEvent.keyDown(summary, { key: 'Enter' });
    expect(details).toHaveAttribute('open');

    // Press Space to collapse
    fireEvent.keyDown(summary, { key: ' ' });
    expect(details).not.toHaveAttribute('open');
  });
});
