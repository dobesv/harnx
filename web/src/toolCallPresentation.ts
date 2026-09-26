/**
 * Maps an assistant-ui tool-call part status to how the ToolCallCard should
 * present it (icon, left-border color, whether it starts expanded).
 *
 * assistant-ui overloads `type: "requires-action"`: a genuine HITL approval
 * uses `reason: "interrupt"`, but a tool call that simply has no result yet
 * gets `reason: "tool-calls"` once it's no longer the last/running message
 * (e.g. when a session is reconstructed in a second tab). Only the interrupt
 * case is a real alert; everything else that lacks a result is "pending".
 */

/**
 * Tool kind categorization. Mirrors harnx_core::event::ToolKind.
 */
export type ToolKind =
  | 'Read'
  | 'Edit'
  | 'Delete'
  | 'Move'
  | 'Search'
  | 'Execute'
  | 'Think'
  | 'Fetch'
  | 'SwitchMode'
  | 'Other';

/**
 * Tool status for live updates. Mirrors harnx_core::event::ToolStatus.
 */
export type ToolStatus = 'Pending' | 'InProgress' | 'Completed' | 'Failed';

const TOOL_KIND_ICONS: Record<ToolKind, string> = {
  Read: '📄',
  Edit: '✏️',
  Delete: '🗑️',
  Move: '📦',
  Search: '🔍',
  Execute: '⚡',
  Think: '💭',
  Fetch: '⬇️',
  SwitchMode: '🔄',
  Other: '🔧',
};

/**
 * Map ToolKind to an icon for visual identification.
 */
export function toolKindToIcon(kind: ToolKind): string {
  return TOOL_KIND_ICONS[kind] ?? '🔧';
}

export type ToolCallPresentation = {
  icon: string;
  borderColor: string;
  defaultExpanded: boolean;
};

export type ToolCallStatusInput = { type?: string; reason?: string } | undefined;

export type ToolCallPresentationOptions = {
  toolName?: string;
  isSubAgent?: boolean;
  /** Live update kind; supersedes default icon when present */
  kind?: ToolKind;
  /** Live update status; used for border color */
  liveStatus?: ToolStatus;
};

type ToolCallStatusFlags = { isPending: boolean; isActionRequired: boolean };

export function isSubAgentTool(toolName?: string): boolean {
  if (!toolName) return false;
  return (
    toolName === 'session_prompt' ||
    toolName.endsWith('_session_prompt') ||
    toolName === 'session_new' ||
    toolName.endsWith('_session_new')
  );
}

export interface ResultExtraction {
  text: string | null;
  isStructuredObject: boolean;
  structuredData: any;
}

const PRIMARY_TEXT_FIELDS = ['response', 'markdown', 'output', 'text', 'result'] as const;

function joinContentArray(content: unknown[]): string | null {
  const text = content
    .map((c: any) => (typeof c === 'string' ? c : c?.text || ''))
    .filter(Boolean)
    .join('\n\n');
  return text || null;
}

function firstStringField(obj: Record<string, any>): string | null {
  for (const field of PRIMARY_TEXT_FIELDS) {
    if (typeof obj[field] === 'string') return obj[field];
  }
  return null;
}

function extractTextFromObject(obj: Record<string, any>): ResultExtraction {
  const field = firstStringField(obj);
  if (field !== null) return { text: field, isStructuredObject: false, structuredData: obj };
  const content = Array.isArray(obj.content) ? joinContentArray(obj.content) : null;
  if (content) return { text: content, isStructuredObject: false, structuredData: obj };
  return { text: null, isStructuredObject: true, structuredData: obj };
}

export function extractResultContent(result: any): ResultExtraction {
  if (result === undefined || result === null) {
    return { text: null, isStructuredObject: false, structuredData: null };
  }

  // Handle strings (plain text, markdown, or JSON string)
  if (typeof result === 'string') {
    try {
      const parsed = JSON.parse(result);
      if (parsed && typeof parsed === 'object') {
        return extractTextFromObject(parsed);
      }
    } catch {
      // Plain text or markdown string
    }
    return { text: result, isStructuredObject: false, structuredData: { result } };
  }

  // Handle objects
  if (typeof result === 'object') {
    return extractTextFromObject(result);
  }

  return { text: String(result), isStructuredObject: false, structuredData: { result } };
}

export function classifyToolCallStatus(status: ToolCallStatusInput): ToolCallStatusFlags {
  const type = status?.type;
  const reason = status?.reason;
  const isActionRequired = type === 'requires-action' && reason === 'interrupt';
  const isPending = type === 'running' || (type === 'requires-action' && !isActionRequired);
  return { isPending, isActionRequired };
}

function toolCallBorderColor(
  status: ToolCallStatusInput,
  flags: ToolCallStatusFlags,
  isError: boolean | undefined,
): string {
  if (flags.isPending) return 'var(--status-running)';
  if (status?.type === 'complete') return 'var(--status-complete)';
  if (flags.isActionRequired) return 'var(--status-action)';
  if (status?.type === 'incomplete' || isError) return 'var(--status-error)';
  return 'var(--border)';
}

function toolCallIcon(flags: ToolCallStatusFlags, isError: boolean | undefined): string {
  if (flags.isPending) return '⏳';
  if (flags.isActionRequired) return '⚠️';
  return isError ? '❌' : '✅';
}

/**
 * Resolve the icon to display for a tool call, considering live updates.
 * Error and action-required states take precedence over kind icon.
 */
function resolveToolCallIcon(
  flags: ToolCallStatusFlags,
  isError: boolean | undefined,
  kind?: ToolKind,
): string {
  // Error and action-required must never be masked by kind icon
  if (flags.isActionRequired) return '⚠️';
  if (isError) return '❌';
  // Kind icon only applies when no terminal state
  if (kind) return toolKindToIcon(kind);
  return toolCallIcon(flags, isError);
}

/**
 * Get border color considering live status updates.
 * Terminal states (complete, error, action-required) take precedence over liveStatus.
 * liveStatus is only consulted while the tool call is still running.
 */
function resolveBorderColor(
  status: ToolCallStatusInput,
  flags: ToolCallStatusFlags,
  isError: boolean | undefined,
  liveStatus?: ToolStatus,
): string {
  // Terminal states take precedence - they must never be masked by stale liveStatus
  if (!flags.isPending || flags.isActionRequired || isError) {
    return toolCallBorderColor(status, flags, isError);
  }
  // Only consult liveStatus while running/pending
  if (liveStatus) {
    switch (liveStatus) {
      case 'Pending':
      case 'InProgress':
        return 'var(--status-running)';
      case 'Completed':
      case 'Failed':
        // Terminal liveStatus - fall through to standard logic
        break;
    }
  }
  return toolCallBorderColor(status, flags, isError);
}

export function getToolCallPresentation(
  status: ToolCallStatusInput,
  isError: boolean | undefined,
  options?: ToolCallPresentationOptions | boolean,
): ToolCallPresentation {
  const flags = classifyToolCallStatus(status);
  const opts = typeof options === 'boolean' ? { isSubAgent: options } : options ?? {};
  const isSubAgent = Boolean(opts.isSubAgent || isSubAgentTool(opts.toolName));

  return {
    icon: resolveToolCallIcon(flags, isError, opts.kind),
    borderColor: resolveBorderColor(status, flags, isError, opts.liveStatus),
    defaultExpanded: flags.isActionRequired || isSubAgent,
  };
}

/**
 * Minimum elapsed time (in milliseconds) before showing a tool-call timer.
 * Timer hides for durations below this threshold, then shows final value on completion.
 */
export const TOOL_TIMER_MIN_ELAPSED_MS = 5000;

/**
 * Format elapsed milliseconds as `{seconds}s` for display.
 * Used by tool-call timers and sub-agent session elapsed displays.
 */
export function formatElapsedMs(valueMs: number): string {
  const seconds = Math.floor(valueMs / 1000);
  return `${seconds}s`;
}
