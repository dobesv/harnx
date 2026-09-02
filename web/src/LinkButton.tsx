import React from 'react';

function isUnmodifiedLeftClick(e: React.MouseEvent): boolean {
  return e.button === 0 && !e.ctrlKey && !e.metaKey && !e.shiftKey && !e.altKey;
}

export interface LinkButtonProps extends Omit<React.AnchorHTMLAttributes<HTMLAnchorElement>, 'href' | 'onClick'> {
  href: string;
  onNavigate: () => void;
  onClick?: React.MouseEventHandler<HTMLAnchorElement>;
}

export const LinkButton = React.forwardRef<HTMLAnchorElement, LinkButtonProps>(
  ({ href, onNavigate, onClick, children, className, ...rest }, ref) => {
    const handleClick = (e: React.MouseEvent<HTMLAnchorElement>) => {
      onClick?.(e);

      if (!e.defaultPrevented && isUnmodifiedLeftClick(e)) {
        e.preventDefault();
        onNavigate();
      }
    };

    return (
      <a ref={ref} href={href} className={className} onClick={handleClick} {...rest}>
        {children}
      </a>
    );
  }
);

LinkButton.displayName = 'LinkButton';
