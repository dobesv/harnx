import { render, screen, fireEvent } from '@testing-library/react';
import { describe, it, expect, vi } from 'vitest';
import { SessionCard, formatSessionContext } from '../SessionCard';
import type { SessionRef } from '../types';

describe('formatSessionContext', () => {
  it('formats repo and branch together', () => {
    expect(formatSessionContext('dobesv/harnx', 'feat/picker')).toBe('dobesv/harnx @ feat/picker');
  });

  it('formats repo only without @ separator', () => {
    expect(formatSessionContext('dobesv/harnx', null)).toBe('dobesv/harnx');
    expect(formatSessionContext('dobesv/harnx', '')).toBe('dobesv/harnx');
    expect(formatSessionContext('dobesv/harnx', undefined)).toBe('dobesv/harnx');
  });

  it('formats branch only as "branch <branch>"', () => {
    expect(formatSessionContext(null, 'feat/picker')).toBe('branch feat/picker');
    expect(formatSessionContext('', 'feat/picker')).toBe('branch feat/picker');
    expect(formatSessionContext(undefined, 'feat/picker')).toBe('branch feat/picker');
  });

  it('returns null when neither is present', () => {
    expect(formatSessionContext(null, null)).toBeNull();
    expect(formatSessionContext('', '')).toBeNull();
    expect(formatSessionContext('   ', '   ')).toBeNull();
    expect(formatSessionContext(undefined, undefined)).toBeNull();
  });
});

describe('SessionCard', () => {
  const baseSession: SessionRef = {
    session_id: 'session-123',
    updated_at: '2026-01-01T12:00:00.000Z',
  };

  it('renders title and repo @ branch context when all are present', () => {
    const session: SessionRef = {
      ...baseSession,
      title: 'Fix issue with session picker',
      repository: 'dobesv/harnx',
      branch: 'fix/picker',
    };

    render(<SessionCard session={session} onSelect={vi.fn()} />);

    expect(screen.getByRole('heading', { level: 3 })).toHaveTextContent('session-123');
    const title = screen.getByTestId('session-title');
    expect(title).toHaveTextContent('Fix issue with session picker');
    const context = screen.getByTestId('session-context');
    expect(context).toHaveTextContent('dobesv/harnx @ fix/picker');
  });

  it('renders repo only without @ when branch is absent', () => {
    const session: SessionRef = {
      ...baseSession,
      title: 'Repo only task',
      repository: 'dobesv/harnx',
      branch: null,
    };

    render(<SessionCard session={session} onSelect={vi.fn()} />);

    const context = screen.getByTestId('session-context');
    expect(context).toHaveTextContent('dobesv/harnx');
    expect(context.textContent).not.toContain('@');
  });

  it('renders branch only as "branch <branch>" when repo is absent', () => {
    const session: SessionRef = {
      ...baseSession,
      title: 'Branch only task',
      repository: null,
      branch: 'feature/async-tools',
    };

    render(<SessionCard session={session} onSelect={vi.fn()} />);

    const context = screen.getByTestId('session-context');
    expect(context).toHaveTextContent('branch feature/async-tools');
    expect(context.textContent).not.toContain('@');
  });

  it('renders no session-context element when neither repo nor branch is present', () => {
    const session: SessionRef = {
      ...baseSession,
      title: 'No context task',
      repository: null,
      branch: null,
    };

    const { container } = render(<SessionCard session={session} onSelect={vi.fn()} />);

    expect(screen.queryByTestId('session-context')).toBeNull();
    expect(container.textContent).not.toContain('null');
    expect(container.textContent).not.toContain('undefined');
  });

  it('renders no session-title element when title is absent or null', () => {
    const session: SessionRef = {
      ...baseSession,
      title: null,
      repository: 'dobesv/harnx',
      branch: 'main',
    };

    const { container } = render(<SessionCard session={session} onSelect={vi.fn()} />);

    expect(screen.queryByTestId('session-title')).toBeNull();
    expect(container.textContent).not.toContain('null');
    expect(container.textContent).not.toContain('undefined');
  });

  it('renders no session-title element when title is empty or whitespace', () => {
    const session: SessionRef = {
      ...baseSession,
      title: '   ',
      repository: null,
      branch: null,
    };

    render(<SessionCard session={session} onSelect={vi.fn()} />);

    expect(screen.queryByTestId('session-title')).toBeNull();
    expect(screen.queryByTestId('session-context')).toBeNull();
  });

  it('handles card selection and unread toggle', () => {
    const onSelect = vi.fn();
    const onToggleUnread = vi.fn();
    const session: SessionRef = {
      ...baseSession,
      unread: true,
    };

    render(
      <SessionCard
        session={session}
        onSelect={onSelect}
        onToggleUnread={onToggleUnread}
      />
    );

    expect(screen.getByTestId('session-unread-badge')).toHaveTextContent('Unread');

    const markReadBtn = screen.getByRole('button', { name: 'Mark session session-123 as read' });
    fireEvent.click(markReadBtn);
    expect(onToggleUnread).toHaveBeenCalledWith('session-123', true);
    expect(onSelect).not.toHaveBeenCalled();

    const mainBtn = screen.getAllByRole('button').find(el => el.classList.contains('session-item-main'))!;
    fireEvent.click(mainBtn);
    expect(onSelect).toHaveBeenCalledWith('session-123');
  });
});
