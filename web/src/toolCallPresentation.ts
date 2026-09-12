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

type ToolCallStatusFlags = { isPending: boolean; isActionRequired: boolean };

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
): ToolCallPresentation {
  const flags = classifyToolCallStatus(status);
  return {
    icon: toolCallIcon(flags, isError),
    borderColor: toolCallBorderColor(status, flags, isError),
    defaultExpanded: flags.isActionRequired,
  };
}
