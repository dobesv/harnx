import type { ToolKind, ToolStatus } from './toolCallPresentation';

/**
 * Token usage snapshot for a tool call.
 * Mirrors harnx_core::api_types::CompletionTokenUsage.
 */
export interface ToolCallUsage {
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cache_write_tokens?: number;
}

/**
 * A file/location affected by a tool call.
 * Mirrors harnx_core::event::ToolLocation.
 */
export interface ToolCallLocation {
  path: string;
  line?: number;
}

/**
 * Patch payload for a tool call update.
 * Mirrors ToolEvent::Update fields from harnx_core::event.
 * Omitted/undefined fields leave current state unchanged.
 * `locations: []` explicitly clears locations.
 */
export interface ToolCallUpdatePatch {
  tool_call_id: string;
  markdown?: string;
  status?: ToolStatus;
  title?: string;
  kind?: ToolKind;
  locations?: ToolCallLocation[];
  usage?: ToolCallUsage;
}

/**
 * State for a single tool call that can receive live updates.
 */
export interface ToolCallState {
  tool_call_id: string;
  markdown?: string;
  status?: ToolStatus;
  title?: string;
  kind?: ToolKind;
  locations: ToolCallLocation[];
  usage?: ToolCallUsage;
}

/**
 * State map for all tracked tool calls, keyed by tool_call_id.
 */
export type ToolUpdatesState = Map<string, ToolCallState>;

/**
 * Initialize state for a tool call when it starts.
 * Used before any Update events arrive.
 */
export function initToolCallState(id: string): ToolCallState {
  return {
    tool_call_id: id,
    locations: [],
  };
}

/**
 * Apply a patch to a tool call state entry.
 * Omitted fields are left unchanged. Empty arrays explicitly clear.
 */
export function applyToolUpdate(
  state: ToolCallState,
  patch: ToolCallUpdatePatch
): ToolCallState {
  return {
    tool_call_id: state.tool_call_id,
    markdown: patch.markdown !== undefined ? patch.markdown : state.markdown,
    status: patch.status !== undefined ? patch.status : state.status,
    title: patch.title !== undefined ? patch.title : state.title,
    kind: patch.kind !== undefined ? patch.kind : state.kind,
    // locations: [] clears; undefined leaves unchanged
    locations: patch.locations !== undefined ? patch.locations : state.locations,
    usage: patch.usage !== undefined ? patch.usage : state.usage,
  };
}

/**
 * State update actions for the tool updates reducer.
 */
export type ToolUpdatesAction =
  | { type: 'TOOL_UPDATE'; patch: ToolCallUpdatePatch }
  | { type: 'TOOL_STARTED'; id: string }
  | { type: 'RESET' };

/**
 * Reducer for tool updates state.
 * Applies updates in-place, creating entries as needed.
 */
export function reduceToolUpdates(
  state: ToolUpdatesState,
  action: ToolUpdatesAction
): ToolUpdatesState {
  switch (action.type) {
    case 'RESET':
      return new Map();
    case 'TOOL_STARTED': {
      const next = new Map(state);
      if (!next.has(action.id)) {
        next.set(action.id, initToolCallState(action.id));
      }
      return next;
    }
    case 'TOOL_UPDATE': {
      const id = action.patch.tool_call_id;
      if (!id) return state;
      const existing = state.get(id);
      const currentState = existing ?? initToolCallState(id);
      const next = new Map(state);
      next.set(id, applyToolUpdate(currentState, action.patch));
      return next;
    }
    default:
      return state;
  }
}

const TOOL_KINDS = new Set<ToolKind>([
  'Read',
  'Edit',
  'Delete',
  'Move',
  'Search',
  'Execute',
  'Think',
  'Fetch',
  'SwitchMode',
  'Other',
]);

const TOOL_STATUSES = new Set<ToolStatus>([
  'Pending',
  'InProgress',
  'Completed',
  'Failed',
]);

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === 'object'
    ? (value as Record<string, unknown>)
    : undefined;
}

function parseLocation(rawLocation: unknown): ToolCallLocation | undefined {
  const location = asRecord(rawLocation);
  if (!location || typeof location.path !== 'string') return undefined;
  if (location.line !== undefined && typeof location.line !== 'number') return undefined;
  return { path: location.path, line: location.line };
}

function isToolCallLocation(
  location: ToolCallLocation | undefined
): location is ToolCallLocation {
  return location !== undefined;
}

function parseLocations(rawLocations: unknown): ToolCallLocation[] | undefined {
  if (!Array.isArray(rawLocations)) return undefined;
  return rawLocations.map(parseLocation).filter(isToolCallLocation);
}

function tokenCount(value: unknown): number {
  return typeof value === 'number' ? value : 0;
}

function optionalTokenCount(value: unknown): number | undefined {
  return typeof value === 'number' ? value : undefined;
}

function parseUsage(rawUsage: unknown): ToolCallUsage | undefined {
  const usage = asRecord(rawUsage);
  if (!usage) return undefined;
  return {
    input_tokens: tokenCount(usage.input_tokens),
    output_tokens: tokenCount(usage.output_tokens),
    cached_tokens: tokenCount(usage.cached_tokens),
    cache_write_tokens: optionalTokenCount(usage.cache_write_tokens),
  };
}

function parseKind(rawKind: unknown): ToolKind | undefined {
  return typeof rawKind === 'string' && TOOL_KINDS.has(rawKind as ToolKind)
    ? (rawKind as ToolKind)
    : undefined;
}

function parseStatus(rawStatus: unknown): ToolStatus | undefined {
  return typeof rawStatus === 'string' && TOOL_STATUSES.has(rawStatus as ToolStatus)
    ? (rawStatus as ToolStatus)
    : undefined;
}

function parseOptionalString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

function parseToolCallId(rawId: unknown): string | undefined {
  const id = parseOptionalString(rawId);
  return id?.trim() ? id : undefined;
}

/**
 * Parse a tool_update custom event payload.
 * Returns undefined if the payload is invalid.
 */
export function parseToolUpdateEvent(value: unknown): ToolCallUpdatePatch | undefined {
  const event = asRecord(value);
  if (!event) return undefined;

  const toolCallId = parseToolCallId(event.tool_call_id);
  if (!toolCallId) return undefined;

  return {
    tool_call_id: toolCallId,
    markdown: parseOptionalString(event.markdown),
    status: parseStatus(event.status),
    title: parseOptionalString(event.title),
    kind: parseKind(event.kind),
    locations: parseLocations(event.locations),
    usage: parseUsage(event.usage),
  };
}
