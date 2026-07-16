/** Base document/tab title used when no session title is present. */
export const DEFAULT_DOCUMENT_TITLE = 'harnx';

/** Format a session title for display in the browser tab. */
export function formatDocumentTitle(title?: string | null): string {
  return title ? `${DEFAULT_DOCUMENT_TITLE} — ${title}` : DEFAULT_DOCUMENT_TITLE;
}

/** Set the browser tab title from an optional session title. */
export function setDocumentTitle(title?: string | null): void {
  document.title = formatDocumentTitle(title);
}
