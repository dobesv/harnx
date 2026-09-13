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

export type ToolCallPresentation = {
  icon: string;
  borderColor: string;
  defaultExpanded: boolean;
};

export type ToolCallStatusInput = { type?: string; reason?: string } | undefined;

export type ToolCallPresentationOptions = {
  toolName?: string;
  isSubAgent?: boolean;
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

function classifyToolCallStatus(status: ToolCallStatusInput): ToolCallStatusFlags {
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

export function getToolCallPresentation(
  status: ToolCallStatusInput,
  isError: boolean | undefined,
  options?: ToolCallPresentationOptions | boolean,
): ToolCallPresentation {
  const flags = classifyToolCallStatus(status);
  const isSubAgent =
    typeof options === 'boolean'
      ? options
      : Boolean(options?.isSubAgent || isSubAgentTool(options?.toolName));

  return {
    icon: toolCallIcon(flags, isError),
    borderColor: toolCallBorderColor(status, flags, isError),
    defaultExpanded: flags.isActionRequired || isSubAgent,
  };
}
