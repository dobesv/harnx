/**
 * Handoff marker extraction from tool results.
 *
 * The engine adds the `handoff` marker to `*_session_handoff` results. This
 * parser accepts any well-formed marker without a tool-name filter, so a tool
 * result that includes one also renders a link (navigation is user-initiated).
 */
export interface HandoffMarker {
  agent: string;
  sessionId: string;
}

/** Returns trimmed string if value is a non-blank string, otherwise null. */
function nonEmptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
}

/** Converts value to a Record if it's a non-null, non-array object, otherwise null. */
function toRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function parseJsonIfString(value: unknown): unknown {
  if (typeof value === 'string') {
    try {
      return JSON.parse(value);
    } catch {
      return null;
    }
  }
  return value;
}

function extractMarkerFromObject(obj: Record<string, unknown>): HandoffMarker | null {
  const marker = toRecord(obj.handoff);
  if (!marker) {
    return null;
  }
  const agent = nonEmptyString(marker.agent);
  const sessionId = nonEmptyString(marker.session_id) ?? nonEmptyString(marker.sessionId);

  if (agent && sessionId) {
    return { agent, sessionId };
  }
  return null;
}

export function parseHandoffMarker(result: unknown): HandoffMarker | null {
  if (result === null || result === undefined) {
    return null;
  }

  const root = toRecord(parseJsonIfString(result));
  if (!root) {
    return null;
  }

  const direct = extractMarkerFromObject(root);
  if (direct) {
    return direct;
  }

  const wrapped = root.result;
  if (wrapped !== undefined && wrapped !== null) {
    const nested = toRecord(parseJsonIfString(wrapped));
    if (nested) {
      return extractMarkerFromObject(nested);
    }
  }

  return null;
}
