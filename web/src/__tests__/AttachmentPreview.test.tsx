import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { vi } from 'vitest';
import { AttachmentPreview } from '../AttachmentPreview';

describe('AttachmentPreview', () => {
  it('shows text as text, traps focus, closes with Escape and restores the opener', async () => {
    const onClose = vi.fn();
    const opener = document.createElement('button');
    const app = document.createElement('div');
    app.className = 'app-container';
    app.append(opener);
    document.body.append(app);
    opener.focus();
    const file = new File(['<script>untrusted</script>\nhello'], 'notes.txt', { type: 'text/plain' });
    const { unmount } = render(<AttachmentPreview file={file} onClose={onClose} />);
    expect(await screen.findByText('<script>untrusted</script> hello')).toBeInTheDocument();
    expect(app).toHaveAttribute('inert');
    const close = screen.getByRole('button', { name: 'Close attachment preview' });
    expect(close).toHaveFocus();
    fireEvent.keyDown(window, { key: 'Tab' });
    expect(close).toHaveFocus();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledOnce();
    unmount();
    expect(app).not.toHaveAttribute('inert');
    expect(opener).toHaveFocus();
    app.remove();
  });

  it('bounds text previews and keeps the full file intact', async () => {
    const file = new File(['a'.repeat(300_000)], 'large.txt', { type: 'text/plain' });
    const { container } = render(<AttachmentPreview file={file} onClose={() => {}} />);
    await screen.findByText('Preview truncated. The full file will be sent.');
    expect(document.querySelector('pre')?.textContent).toHaveLength(256 * 1024);
    expect(file.size).toBe(300_000);
    expect(container.querySelector('pre')).toBeNull(); // Dialog lives outside the inert app.
  });

  it('omits a UTF-8 character split by the preview limit', async () => {
    const prefix = 'a'.repeat(256 * 1024 - 1);
    const file = new File([prefix + 'é'], 'boundary.txt', { type: 'text/plain' });
    render(<AttachmentPreview file={file} onClose={() => {}} />);
    await screen.findByText('Preview truncated. The full file will be sent.');
    expect(document.querySelector('pre')?.textContent).toBe(prefix);
  });

  it('previews images and revokes their object URLs on close', async () => {
    const create = vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:preview');
    const revoke = vi.spyOn(URL, 'revokeObjectURL');
    const file = new File(['image'], 'photo.png', { type: 'image/png' });
    const { unmount } = render(<AttachmentPreview file={file} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByRole('img', { name: 'photo.png' })).toHaveAttribute('src', 'blob:preview'));
    unmount();
    expect(revoke).toHaveBeenCalledWith('blob:preview');
    create.mockRestore();
    revoke.mockRestore();
  });

  it('offers confirmed removal from the preview and makes PDFs keyboard reachable', () => {
    const remove = vi.fn();
    const file = new File(['pdf'], 'report.pdf', { type: 'application/pdf' });
    const { unmount } = render(<AttachmentPreview file={file} onClose={() => {}} onRemove={remove} />);
    expect(screen.getByTitle('report.pdf')).toHaveAttribute('tabindex', '0');
    fireEvent.click(screen.getByRole('button', { name: 'Remove report.pdf' }));
    expect(remove).toHaveBeenCalledOnce();
    fireEvent.error(screen.getByTitle('report.pdf'));
    expect(screen.getByRole('alert')).toHaveTextContent('Unable to preview attachment.');
    unmount();
  });

  it('does not render Office binary formats as text', () => {
    const file = new File(['PK binary zip content'], 'report.docx', {
      type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
    });
    render(<AttachmentPreview file={file} onClose={() => {}} />);
    expect(screen.getByText('Preview unavailable for this file type.')).toBeInTheDocument();
    expect(document.querySelector('pre')).toBeNull();
  });
});
