import React from 'react';

export interface MarkdownLinkProps extends React.AnchorHTMLAttributes<HTMLAnchorElement> {
  node?: unknown;
}

export const MarkdownLink: React.FC<MarkdownLinkProps> = ({
  node: _node,
  children,
  href,
  onClick,
  onKeyDown,
  ...props
}) => (
  <a
    href={href}
    target="_blank"
    rel="noopener noreferrer"
    onClick={(e) => {
      onClick?.(e);
      e.stopPropagation();
    }}
    onKeyDown={(e) => {
      onKeyDown?.(e);
      if (e.key === 'Enter' || e.key === ' ') {
        e.stopPropagation();
      }
    }}
    {...props}
  >
    {children}
  </a>
);
