import React from 'react';
import { render, screen, fireEvent, createEvent } from '@testing-library/react';
import { describe, it, expect, vi } from 'vitest';
import { LinkButton } from './LinkButton';

describe('LinkButton', () => {
  it('renders an anchor with the given href and forwards ref', () => {
    const ref = React.createRef<HTMLAnchorElement>();
    render(
      <LinkButton href="/test" onNavigate={() => {}} ref={ref}>
        Click me
      </LinkButton>
    );
    const link = screen.getByText('Click me');
    expect(link.tagName).toBe('A');
    expect(link.getAttribute('href')).toBe('/test');
    expect(ref.current).toBe(link);
  });

  it('calls onNavigate exactly once and prevents default on unmodified left-click', () => {
    const onNavigate = vi.fn();
    render(
      <LinkButton href="/test" onNavigate={onNavigate}>
        Click me
      </LinkButton>
    );
    const link = screen.getByText('Click me');
    const event = createEvent.click(link, { button: 0 });
    fireEvent(link, event);
    expect(onNavigate).toHaveBeenCalledTimes(1);
    expect(event.defaultPrevented).toBe(true);
  });

  const modifiers = ['ctrlKey', 'metaKey', 'shiftKey', 'altKey'] as const;
  modifiers.forEach((modifier) => {
    it(`does not call onNavigate or prevent default with ${modifier}`, () => {
      const onNavigate = vi.fn();
      render(
        <LinkButton href="/test" onNavigate={onNavigate}>
          Click me
        </LinkButton>
      );
      const link = screen.getByText('Click me');
      const event = createEvent.click(link, { button: 0, [modifier]: true });
      fireEvent(link, event);
      expect(onNavigate).not.toHaveBeenCalled();
      expect(event.defaultPrevented).toBe(false);
    });
  });

  it('runs injected onClick first; if default prevented, onNavigate is not called', () => {
    const onNavigate = vi.fn();
    const onClick = vi.fn((e: React.MouseEvent) => e.preventDefault());
    render(
      <LinkButton href="/test" onNavigate={onNavigate} onClick={onClick}>
        Click me
      </LinkButton>
    );

    const link = screen.getByText('Click me');
    const event = createEvent.click(link, { button: 0 });
    fireEvent(link, event);

    expect(onClick).toHaveBeenCalledTimes(1);
    expect(onNavigate).not.toHaveBeenCalled();
  });

  it('runs injected onClick first; if default not prevented, onNavigate is called', () => {
    const onNavigate = vi.fn();
    const onClick = vi.fn();
    render(
      <LinkButton href="/test" onNavigate={onNavigate} onClick={onClick}>
        Click me
      </LinkButton>
    );

    const link = screen.getByText('Click me');
    const event = createEvent.click(link, { button: 0 });
    fireEvent(link, event);

    expect(onClick).toHaveBeenCalledTimes(1);
    expect(onNavigate).toHaveBeenCalledTimes(1);
  });
});