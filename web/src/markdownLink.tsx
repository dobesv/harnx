import React from 'react';
import { getCidUrl } from './api';
import { type CidDocumentTarget, useCidDocument } from './CidDocumentContext';

const TEXT_FILE_EXTENSION = /\.(?:md|markdown|mdown|mkd|txt|text|json|ya?ml|toml|csv|tsv|xml|html?|css|jsx?|tsx?|py|rs|go|java|c|cc|cpp|h|hpp|sh|bash|zsh|fish|sql|log)$/i;

function textFromChildren(children: React.ReactNode): string {
  let text = '';
  React.Children.forEach(children, (child) => {
    if (typeof child === 'string' || typeof child === 'number') {
      text += String(child);
    } else if (React.isValidElement<{ children?: React.ReactNode }>(child)) {
      text += textFromChildren(child.props.children);
    }
  });
  return text.trim();
}

function isTextMimeType(type: string | undefined): boolean {
  if (!type) return false;
  const mime = type.split(';', 1)[0].trim().toLowerCase();
  return mime.startsWith('text/') || [
    'application/json',
    'application/ld+json',
    'application/xml',
    'application/yaml',
    'application/x-yaml',
    'application/toml',
  ].includes(mime);
}

function isTextMediaLink(
  href: string,
  type: string | undefined,
  download: string | boolean | undefined,
  children: React.ReactNode,
): boolean {
  if (!href.startsWith('cid:media:')) return false;
  if (isTextMimeType(type)) return true;

  const name = typeof download === 'string' ? download : textFromChildren(children);
  return TEXT_FILE_EXTENSION.test(name);
}

interface LinkBehavior {
  href: string | undefined;
  isCidDocument: boolean;
  rel: string | undefined;
  target: React.HTMLAttributeAnchorTarget | undefined;
}

type LinkBehaviorInput = Pick<
  MarkdownLinkProps,
  'children' | 'download' | 'href' | 'mimeType' | 'rel' | 'target' | 'type'
>;

function isCidMediaLink(href: string | undefined): boolean {
  if (!href) return false;
  return href.startsWith('cid:media:');
}

function isCidDocumentLink({
  children,
  download,
  href,
  mimeType,
  type,
}: LinkBehaviorInput): boolean {
  if (!href) return false;
  if (href.startsWith('cid:plan:')) return true;
  return isTextMediaLink(href, mimeType ?? type, download, children);
}

function isExternalLink(href: string | undefined): boolean {
  if (!href) return false;
  return /^https?:\/\//i.test(href);
}

function resolvedLinkHref(
  href: string | undefined,
  isCidMedia: boolean,
  isCidDocument: boolean,
): string | undefined {
  if (!href) return href;
  if (!isCidMedia) return href;
  if (isCidDocument) return href;
  return getCidUrl(href);
}

function computeLinkBehavior(input: LinkBehaviorInput): LinkBehavior {
  const isCidMedia = isCidMediaLink(input.href);
  const isCidDocument = isCidDocumentLink(input);
  const opensNewTab = isCidMedia ? !isCidDocument : isExternalLink(input.href);
  let { target, rel } = input;
  if (opensNewTab) {
    target = '_blank';
    rel = 'noopener noreferrer';
  }

  return {
    href: resolvedLinkHref(input.href, isCidMedia, isCidDocument),
    isCidDocument,
    rel,
    target,
  };
}

interface LinkClickOptions {
  behavior: LinkBehavior;
  documentTitle: string;
  href: string | undefined;
  onClick: React.MouseEventHandler<HTMLAnchorElement> | undefined;
  openCid: ((target: CidDocumentTarget) => void) | undefined;
}

function handleLinkClick(
  event: React.MouseEvent<HTMLAnchorElement>,
  { behavior, documentTitle, href, onClick, openCid }: LinkClickOptions,
): void {
  onClick?.(event);
  event.stopPropagation();
  if (event.defaultPrevented) return;
  if (!behavior.isCidDocument) return;
  if (!href) return;
  if (!openCid) return;
  event.preventDefault();
  openCid({ cid: href, title: documentTitle || href });
}

function handleLinkKeyDown(
  event: React.KeyboardEvent<HTMLAnchorElement>,
  onKeyDown: React.KeyboardEventHandler<HTMLAnchorElement> | undefined,
  isCidDocument: boolean,
): void {
  onKeyDown?.(event);
  if (event.defaultPrevented) return;
  if (event.key === 'Enter') {
    event.stopPropagation();
  } else if (event.key === ' ' && isCidDocument) {
    event.preventDefault();
    event.stopPropagation();
    event.currentTarget.click();
  }
}

export interface MarkdownLinkProps extends React.AnchorHTMLAttributes<HTMLAnchorElement> {
  node?: unknown;
  mimeType?: string;
  onOpenCid?: (target: CidDocumentTarget) => void;
}

export const MarkdownLink: React.FC<MarkdownLinkProps> = ({
  node: _node,
  children,
  href,
  onClick,
  onKeyDown,
  onOpenCid,
  mimeType,
  target,
  rel,
  type,
  download,
  ...props
}) => {
  const cidDocument = useCidDocument();
  const openCid = onOpenCid ?? cidDocument?.openDocument;
  const behavior = computeLinkBehavior({ children, download, href, mimeType, rel, target, type });
  const clickOptions = {
    behavior,
    documentTitle: textFromChildren(children),
    href,
    onClick,
    openCid,
  };

  return (
    <a
      href={behavior.href}
      target={behavior.target}
      rel={behavior.rel}
      type={type}
      download={download}
      onClick={(event) => handleLinkClick(event, clickOptions)}
      onKeyDown={(event) => handleLinkKeyDown(event, onKeyDown, behavior.isCidDocument)}
      {...props}
    >
      {children}
    </a>
  );
};
