export type ThinkSegment =
  | { type: 'text'; text: string }
  | { type: 'thought'; text: string };

export const THINK_TAG_PAIRS = [
  { open: '<think>', close: '</think>' },
  { open: '<thinking>', close: '</thinking>' },
] as const;

export interface ThinkBlockMatch {
  index: number;
  open: string;
  close: string;
}

/**
 * Finds the earliest matching opening tag among supported think tag pairs.
 */
function findEarliestOpener(text: string): ThinkBlockMatch | null {
  let earliest: ThinkBlockMatch | null = null;
  for (const pair of THINK_TAG_PAIRS) {
    const idx = text.indexOf(pair.open);
    if (idx !== -1 && (!earliest || idx < earliest.index)) {
      earliest = { index: idx, open: pair.open, close: pair.close };
    }
  }
  return earliest;
}

export function hasThinkTags(text: string): boolean {
  return THINK_TAG_PAIRS.some(
    (pair) => text.includes(pair.open) || text.includes(pair.close),
  );
}

export interface SplitResult {
  segments: ThinkSegment[];
  unclosedOpener: { index: number; open: string; close: string } | null;
}

/**
 * Single-pass scan that extracts think segments and identifies any unclosed trailing opener.
 */
export function scanThinkBlocks(text: string): SplitResult {
  const segments: ThinkSegment[] = [];
  let remaining = text;
  let offset = 0;
  let unclosedOpener: { index: number; open: string; close: string } | null = null;

  while (remaining.length > 0) {
    const earliestMatch = findEarliestOpener(remaining);

    if (!earliestMatch) {
      if (remaining.length > 0) {
        segments.push({ type: 'text', text: remaining });
      }
      break;
    }

    if (earliestMatch.index > 0) {
      segments.push({
        type: 'text',
        text: remaining.slice(0, earliestMatch.index),
      });
    }

    const content = remaining.slice(earliestMatch.index + earliestMatch.open.length);
    const closeIdx = content.indexOf(earliestMatch.close);

    let thought = closeIdx !== -1 ? content.slice(0, closeIdx) : content;
    // Removes at most one '\n' immediately after an opener
    if (thought.startsWith('\n')) {
      thought = thought.slice(1);
    }
    // and immediately before a matching closer
    if (closeIdx !== -1) {
      if (thought.endsWith('\n')) {
        thought = thought.slice(0, -1);
      }
    }

    if (thought.length > 0) {
      segments.push({ type: 'thought', text: thought });
    }

    if (closeIdx !== -1) {
      const advance = earliestMatch.index + earliestMatch.open.length + closeIdx + earliestMatch.close.length;
      remaining = remaining.slice(advance);
      offset += advance;
    } else {
      unclosedOpener = {
        index: offset + earliestMatch.index,
        open: earliestMatch.open,
        close: earliestMatch.close,
      };
      break;
    }
  }

  return { segments, unclosedOpener };
}

export function splitThinkBlocks(text: string): ThinkSegment[] {
  return scanThinkBlocks(text).segments;
}

export function hasUnclosedThinkOpener(text: string): boolean {
  return scanThinkBlocks(text).unclosedOpener !== null;
}

export function resolveThinkSegments(
  text: string,
  options?: { isStreaming?: boolean },
): ThinkSegment[] {
  const { segments, unclosedOpener } = scanThinkBlocks(text);
  if (options?.isStreaming && unclosedOpener) {
    // Content after the unclosed opener
    let trailingThoughtContent = text.slice(unclosedOpener.index + unclosedOpener.open.length);
    if (trailingThoughtContent.startsWith('\n')) {
      trailingThoughtContent = trailingThoughtContent.slice(1);
    }
    if (trailingThoughtContent.length === 0) {
      return [...segments, { type: 'thought', text: '' }];
    }
  }
  return segments;
}
