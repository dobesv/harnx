import { useMemo, useState } from 'react';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { CidDocumentContext, type CidDocumentTarget } from './CidDocumentContext';
import { CidDocumentViewer } from './CidDocumentViewer';
import { MarkdownLink } from './markdownLink';
import { fetchCidContent } from './api';

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    fetchCidContent: vi.fn(),
  };
});

const PLAN = 'cid:plan:_temp/abc123/project';
const TASK = 'cid:plan:_temp/abc123/project/tasks/build';
const MEDIA = `cid:media:_temp/abc123/${'a'.repeat(64)}`;

function ViewerHarness() {
  const [target, setTarget] = useState<CidDocumentTarget | null>(null);
  const context = useMemo(() => ({ openDocument: setTarget }), []);
  return (
    <CidDocumentContext.Provider value={context}>
      <MarkdownLink href={PLAN}>Project plan</MarkdownLink>
      {target ? (
        <CidDocumentViewer
          cid={target.cid}
          title={target.title}
          onClose={() => setTarget(null)}
        />
      ) : null}
    </CidDocumentContext.Provider>
  );
}

describe('CID links and document viewer', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it('opens a plan link in the viewer and fetches its markdown', async () => {
    vi.mocked(fetchCidContent).mockResolvedValue({
      mimeType: 'text/markdown; charset=utf-8',
      text: '# Project\n\nPlan body.',
      etag: '"3"',
    });
    render(<ViewerHarness />);

    fireEvent.click(screen.getByRole('link', { name: 'Project plan' }));

    expect(await screen.findByRole('dialog', { name: 'Project plan' })).toBeInTheDocument();
    await waitFor(() => expect(fetchCidContent).toHaveBeenCalledWith(PLAN));
    expect(await screen.findByRole('heading', { name: 'Project', level: 1 })).toBeInTheDocument();
  });

  it('rewrites binary media links to the CID endpoint', () => {
    render(<MarkdownLink href={MEDIA}>diagram.png</MarkdownLink>);

    const link = screen.getByRole('link', { name: 'diagram.png' });
    expect(link).toHaveAttribute('href', `/v1/cid/${encodeURIComponent(MEDIA)}`);
    expect(link).toHaveAttribute('target', '_blank');
    expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  });

  it('navigates to plan items inside the viewer and goes back', async () => {
    vi.mocked(fetchCidContent).mockImplementation(async (cid) => {
      if (cid === PLAN) {
        return {
          mimeType: 'text/markdown',
          text: `# Project\n\n[Build task](${TASK})`,
        };
      }
      return {
        mimeType: 'text/markdown',
        text: '# Build task\n\nTask details.',
      };
    });

    render(<CidDocumentViewer cid={PLAN} title="Project plan" onClose={vi.fn()} />);
    fireEvent.click(await screen.findByRole('link', { name: 'Build task' }));

    expect(await screen.findByRole('heading', { name: 'Build task', level: 1 })).toBeInTheDocument();
    await waitFor(() => expect(fetchCidContent).toHaveBeenLastCalledWith(TASK));

    fireEvent.click(screen.getByRole('button', { name: 'Back' }));
    expect(await screen.findByRole('heading', { name: 'Project', level: 1 })).toBeInTheDocument();
    await waitFor(() => expect(fetchCidContent).toHaveBeenLastCalledWith(PLAN));
  });

  it('closes from Escape or the close button', async () => {
    vi.mocked(fetchCidContent).mockResolvedValue({ mimeType: 'text/markdown', text: '# Project' });
    const onClose = vi.fn();
    const { rerender } = render(
      <CidDocumentViewer cid={PLAN} title="Project plan" onClose={onClose} />,
    );
    await screen.findByRole('heading', { name: 'Project', level: 1 });

    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);

    onClose.mockClear();
    rerender(<CidDocumentViewer cid={PLAN} title="Project plan" onClose={onClose} />);
    fireEvent.click(screen.getByRole('button', { name: 'Close document viewer' }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('opens a text media link when its MIME type is supplied', () => {
    const openDocument = vi.fn();
    render(
      <CidDocumentContext.Provider value={{ openDocument }}>
        <MarkdownLink href={MEDIA} type="text/plain">notes</MarkdownLink>
      </CidDocumentContext.Provider>,
    );

    fireEvent.click(screen.getByRole('link', { name: 'notes' }));
    expect(openDocument).toHaveBeenCalledWith({ cid: MEDIA, title: 'notes' });
  });
});
