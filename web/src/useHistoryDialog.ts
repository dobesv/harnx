import { useEffect, type RefObject } from 'react';

export const FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function shouldWrapToLast(current: Element | null, first: HTMLElement, dialog: HTMLElement): boolean {
  return current === first || !dialog.contains(current);
}

function shouldWrapToFirst(current: Element | null, last: HTMLElement, dialog: HTMLElement): boolean {
  return current === last || !dialog.contains(current);
}

function trapFocusDirection(
  event: KeyboardEvent,
  dialog: HTMLElement,
  firstElement: HTMLElement,
  lastElement: HTMLElement,
): void {
  const currentActive = globalThis.document.activeElement;
  if (event.shiftKey && shouldWrapToLast(currentActive, firstElement, dialog)) {
    event.preventDefault();
    lastElement.focus();
    return;
  }
  if (!event.shiftKey && shouldWrapToFirst(currentActive, lastElement, dialog)) {
    event.preventDefault();
    firstElement.focus();
  }
}

function handleDialogTab(event: KeyboardEvent, dialog: HTMLElement | null): void {
  if (!dialog) return;
  const focusables = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR));
  if (focusables.length === 0) {
    event.preventDefault();
    return;
  }
  trapFocusDirection(event, dialog, focusables[0], focusables[focusables.length - 1]);
}

function useDialogEscapeAndTab(
  dialogRef: RefObject<HTMLElement | null>,
  onClose: () => void,
): void {
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        onClose();
        return;
      }
      if (event.key === 'Tab') {
        handleDialogTab(event, dialogRef.current);
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [dialogRef, onClose]);
}

function restoreAttribute(el: HTMLElement, attr: string, value: string | null | undefined): void {
  if (value !== null && value !== undefined) {
    el.setAttribute(attr, value);
  } else {
    el.removeAttribute(attr);
  }
}

function useDialogInertBackdrop(): void {
  useEffect(() => {
    const appContainer = globalThis.document.querySelector<HTMLElement>('.app-container');
    const prevInert = appContainer?.getAttribute('inert');
    const prevAriaHidden = appContainer?.getAttribute('aria-hidden');
    const prevBodyOverflow = globalThis.document.body.style.overflow;

    if (appContainer) {
      appContainer.setAttribute('inert', '');
      appContainer.setAttribute('aria-hidden', 'true');
    }
    globalThis.document.body.style.overflow = 'hidden';

    return () => {
      if (appContainer) {
        restoreAttribute(appContainer, 'inert', prevInert);
        restoreAttribute(appContainer, 'aria-hidden', prevAriaHidden);
      }
      globalThis.document.body.style.overflow = prevBodyOverflow;
    };
  }, []);
}

function useDialogInitialFocus(closeButtonRef: RefObject<HTMLButtonElement | null>): void {
  useEffect(() => {
    const previouslyFocused = globalThis.document.activeElement;
    closeButtonRef.current?.focus();
    return () => {
      if (previouslyFocused instanceof HTMLElement) previouslyFocused.focus();
    };
  }, [closeButtonRef]);
}

export function useHistoryDialog(
  dialogRef: RefObject<HTMLElement | null>,
  closeButtonRef: RefObject<HTMLButtonElement | null>,
  onClose: () => void,
): void {
  useDialogInitialFocus(closeButtonRef);
  useDialogEscapeAndTab(dialogRef, onClose);
  useDialogInertBackdrop();
}
