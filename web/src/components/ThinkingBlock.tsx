import React, { useState } from 'react';

export interface ThinkingBlockProps {
  /** Thought content to display */
  text?: string;
  /** Whether the part is currently streaming. */
  isStreaming?: boolean;
  className?: string;
}

export const ThinkingBlock: React.FC<ThinkingBlockProps> = ({
  text = '',
  isStreaming = false,
  className = '',
}) => {
  // null = follow isStreaming automatically; boolean = explicit user toggle.
  // The <summary> click/keydown handlers preventDefault and set this override
  // directly: Chromium and jsdom also fire `toggle` for programmatic `open`
  // changes, so a toggle event alone can't distinguish user intent from the
  // automatic open-while-streaming.
  const [userOpen, setUserOpen] = useState<boolean | null>(null);

  const open = userOpen ?? isStreaming;

  if (!text && !isStreaming) {
    return null;
  }

  const handleToggle = (e: React.MouseEvent<HTMLElement> | React.KeyboardEvent<HTMLElement>) => {
    e.preventDefault();
    setUserOpen(!open);
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLElement>) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      setUserOpen(!open);
    }
  };

  return (
    <details
      className={`aui-thinking-block ${className}`.trim()}
      open={open}
      onToggle={(e) => {
        // Native toggle fires on user interaction if not prevented, or programmatically in some browsers.
        // Sync userOpen with element state if user interacted directly without clicking summary
        const detailsEl = e.currentTarget as HTMLDetailsElement;
        if (detailsEl.open !== open) {
          setUserOpen(detailsEl.open);
        }
      }}
    >
      <summary
        className="aui-thinking-summary"
        onClick={handleToggle}
        onKeyDown={handleKeyDown}
      >
        <span className="aui-thinking-title">Thinking</span>
        {isStreaming && <span className="aui-thinking-dot" aria-hidden="true">…</span>}
        <span className="aui-thinking-marker" aria-hidden="true">
          {open ? '▾' : '▸'}
        </span>
      </summary>
      <div className="aui-thinking-content">
        {text}
      </div>
    </details>
  );
};
