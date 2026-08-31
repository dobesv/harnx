export type SubAgentNoteStatus = 'running' | 'done' | 'failed';

export interface SubAgentNote {
  id: string;
  invocationId?: string;
  agent: string;
  sessionId: string;
  parentMessageId: string;
  status: SubAgentNoteStatus;
  elapsedMs: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  toolCallCount: number;
  updatedAtMs: number;
}

export interface SubAgentNotesState {
  notes: SubAgentNote[];
  latestParentMessageId: string | null;
  nextId: number;
}

export const INITIAL_SUB_AGENT_NOTES_STATE: SubAgentNotesState = {
  notes: [],
  latestParentMessageId: null,
  nextId: 0,
};

type EventRecord = Record<string, unknown>;

interface SubAgentIdentity {
  agent: string;
  sessionId: string;
  invocationId?: string;
}

interface SubAgentProgressValue extends SubAgentIdentity {
  invocationId: string;
  status: SubAgentNoteStatus;
  elapsedMs: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  toolCallCount: number;
}

type ProgressMetrics = Pick<
  SubAgentProgressValue,
  'elapsedMs' | 'inputTokens' | 'outputTokens' | 'cachedTokens' | 'toolCallCount'
>;

type EventReducer = (
  state: SubAgentNotesState,
  event: EventRecord,
) => SubAgentNotesState;

function record(value: unknown): EventRecord | undefined {
  return value !== null && typeof value === 'object' ? value as EventRecord : undefined;
}

function nonBlankString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim().length > 0 ? value : undefined;
}

function nonNegativeNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0
    ? value
    : undefined;
}

function eventType(event: unknown): string | undefined {
  return nonBlankString(record(event)?.type);
}

function eventRecords(value: unknown): EventRecord[] {
  if (!Array.isArray(value)) return [];
  return value
    .map(record)
    .filter((entry): entry is EventRecord => entry !== undefined);
}

function subAgentIdentity(
  agentValue: unknown,
  sessionIdValue: unknown,
  invocationIdValue?: unknown,
): SubAgentIdentity | undefined {
  const agent = nonBlankString(agentValue);
  if (!agent) return undefined;
  const sessionId = nonBlankString(sessionIdValue);
  if (!sessionId) return undefined;
  return {
    agent,
    sessionId,
    invocationId: nonBlankString(invocationIdValue),
  };
}

function subAgentMarker(value: unknown): SubAgentIdentity | undefined {
  const marker = record(record(value)?.sub_agent);
  return subAgentIdentity(marker?.agent, marker?.session_id);
}

function progressStatus(value: unknown): SubAgentNoteStatus | undefined {
  if (typeof value !== 'string') return undefined;
  if (!['running', 'done', 'failed'].includes(value)) return undefined;
  return value as SubAgentNoteStatus;
}

function progressMetrics(progress: EventRecord): ProgressMetrics | undefined {
  const usage = record(progress.usage);
  const values = [
    nonNegativeNumber(progress.elapsed_ms),
    nonNegativeNumber(usage?.input_tokens),
    nonNegativeNumber(usage?.output_tokens),
    nonNegativeNumber(usage?.cached_tokens),
    nonNegativeNumber(progress.tool_call_count),
  ];
  if (values.some((value) => value === undefined)) return undefined;
  const [elapsedMs, inputTokens, outputTokens, cachedTokens, toolCallCount] = values as number[];
  return { elapsedMs, inputTokens, outputTokens, cachedTokens, toolCallCount };
}

function subAgentProgress(value: unknown): SubAgentProgressValue | undefined {
  const progress = record(value);
  if (!progress) return undefined;
  const identity = subAgentIdentity(
    progress.agent,
    progress.session_id,
    progress.invocation_id,
  );
  if (!identity?.invocationId) return undefined;
  const status = progressStatus(progress.status);
  if (!status) return undefined;
  const metrics = progressMetrics(progress);
  if (!metrics) return undefined;
  return {
    ...identity,
    ...metrics,
    invocationId: identity.invocationId,
    status,
  };
}

function resultValue(content: unknown): EventRecord | undefined {
  if (typeof content !== 'string') return record(content);
  try {
    return record(JSON.parse(content));
  } catch {
    return undefined;
  }
}

function resultMarker(content: unknown): SubAgentIdentity | undefined {
  return subAgentMarker(resultValue(content));
}

function resultProgress(content: unknown): SubAgentProgressValue | undefined {
  return subAgentProgress(resultValue(content)?.sub_agent_progress);
}

function toolCalls(message: EventRecord): EventRecord[] {
  return eventRecords(message.toolCalls ?? message.tool_calls);
}

function toolCallId(value: EventRecord): string | undefined {
  return nonBlankString(value.toolCallId ?? value.tool_call_id ?? value.id);
}

function parentEntry(
  parentMessageId: string,
  call: EventRecord,
): Array<[string, string]> {
  const id = toolCallId(call);
  return id ? [[id, parentMessageId]] : [];
}

function parentEntries(message: EventRecord): Array<[string, string]> {
  if (nonBlankString(message.role) !== 'assistant') return [];
  const parentMessageId = nonBlankString(message.id);
  if (!parentMessageId) return [];
  return toolCalls(message).flatMap((call) => parentEntry(parentMessageId, call));
}

function snapshotNote(
  message: EventRecord,
  parentByToolCall: Map<string, string>,
): SubAgentNote | undefined {
  const context = snapshotContext(message, parentByToolCall);
  if (!context) return undefined;
  const progress = resultProgress(message.content);
  const marker = progress ?? resultMarker(message.content);
  if (!marker) return undefined;
  const metrics = restoredMetrics(progress);
  return {
    id: `snapshot:${context.parentMessageId}:${context.callId}`,
    invocationId: progress?.invocationId,
    agent: marker.agent,
    sessionId: marker.sessionId,
    parentMessageId: context.parentMessageId,
    status: progress?.status ?? 'done',
    ...metrics,
    updatedAtMs: Date.now(),
  };
}

function snapshotContext(
  message: EventRecord,
  parentByToolCall: Map<string, string>,
): { callId: string; parentMessageId: string } | undefined {
  if (nonBlankString(message.role) !== 'tool') return undefined;
  const callId = toolCallId(message);
  if (!callId) return undefined;
  const parentMessageId = parentByToolCall.get(callId);
  return parentMessageId ? { callId, parentMessageId } : undefined;
}

function restoredMetrics(progress: SubAgentProgressValue | undefined): ProgressMetrics {
  if (!progress) {
    return { elapsedMs: 0, inputTokens: 0, outputTokens: 0, cachedTokens: 0, toolCallCount: 0 };
  }
  const { elapsedMs, inputTokens, outputTokens, cachedTokens, toolCallCount } = progress;
  return { elapsedMs, inputTokens, outputTokens, cachedTokens, toolCallCount };
}

function optionalNote(note: SubAgentNote | undefined): SubAgentNote[] {
  return note ? [note] : [];
}
function uniqueNotes(notes: SubAgentNote[]): SubAgentNote[] {
  const byId = new Map(notes.map((note) => [note.id, note]));
  return [...byId.values()];
}

function notesFromSnapshot(messages: unknown): SubAgentNote[] {
  const entries = eventRecords(messages);
  const parentByToolCall = new Map(entries.flatMap(parentEntries));
  const notes = entries.flatMap((message) => (
    optionalNote(snapshotNote(message, parentByToolCall))
  ));
  return uniqueNotes(notes);
}

function startedIdentity(value: unknown): SubAgentIdentity | undefined {
  const marker = record(value);
  return subAgentIdentity(marker?.agent, marker?.session_id, marker?.invocation_id);
}

function sameIdentity(note: SubAgentNote, identity: SubAgentIdentity): boolean {
  if (note.agent !== identity.agent) return false;
  return note.sessionId === identity.sessionId;
}

function isDuplicateNote(
  note: SubAgentNote,
  identity: SubAgentIdentity,
  parentMessageId: string,
): boolean {
  if (note.parentMessageId !== parentMessageId) return false;
  if (identity.invocationId) return note.invocationId === identity.invocationId;
  if (note.status !== 'running') return false;
  return sameIdentity(note, identity);
}

function hasDuplicateNote(
  state: SubAgentNotesState,
  identity: SubAgentIdentity,
  parentMessageId: string,
): boolean {
  return state.notes.some((note) => (
    isDuplicateNote(note, identity, parentMessageId)
  ));
}

function runningNote(
  state: SubAgentNotesState,
  identity: SubAgentIdentity,
  parentMessageId: string,
): SubAgentNote {
  return {
    id: identity.invocationId ? `live:${identity.invocationId}` : `live:${state.nextId}`,
    invocationId: identity.invocationId,
    agent: identity.agent,
    sessionId: identity.sessionId,
    parentMessageId,
    status: 'running',
    elapsedMs: 0,
    inputTokens: 0,
    outputTokens: 0,
    cachedTokens: 0,
    toolCallCount: 0,
    updatedAtMs: Date.now(),
  };
}

function startNote(state: SubAgentNotesState, value: unknown): SubAgentNotesState {
  const identity = startedIdentity(value);
  if (!identity) return state;
  const parentMessageId = state.latestParentMessageId;
  if (!parentMessageId) return state;
  if (hasDuplicateNote(state, identity, parentMessageId)) return state;

  return {
    ...state,
    notes: [...state.notes, runningNote(state, identity, parentMessageId)],
    nextId: state.nextId + 1,
  };
}

function isMatchingRunningNote(
  note: SubAgentNote,
  identity: SubAgentIdentity,
): boolean {
  if (note.status !== 'running') return false;
  if (identity.invocationId) return note.invocationId === identity.invocationId;
  return sameIdentity(note, identity);
}

function noteFromProgress(
  progress: SubAgentProgressValue,
  parentMessageId: string,
): SubAgentNote {
  return {
    id: `live:${progress.invocationId}`,
    invocationId: progress.invocationId,
    agent: progress.agent,
    sessionId: progress.sessionId,
    parentMessageId,
    status: progress.status,
    elapsedMs: progress.elapsedMs,
    inputTokens: progress.inputTokens,
    outputTokens: progress.outputTokens,
    cachedTokens: progress.cachedTokens,
    toolCallCount: progress.toolCallCount,
    updatedAtMs: Date.now(),
  };
}

function applyProgress(
  state: SubAgentNotesState,
  value: unknown,
): SubAgentNotesState {
  const progress = subAgentProgress(value);
  if (!progress) return state;
  const index = state.notes.findIndex((note) => note.invocationId === progress.invocationId);
  if (index >= 0) {
    if (state.notes[index].status !== 'running') return state;
    return {
      ...state,
      notes: state.notes.map((note, noteIndex) => (
        noteIndex === index
          ? { ...note, ...noteFromProgress(progress, note.parentMessageId), id: note.id }
          : note
      )),
    };
  }
  const parentMessageId = state.latestParentMessageId;
  if (!parentMessageId) return state;
  return {
    ...state,
    notes: [...state.notes, noteFromProgress(progress, parentMessageId)],
    nextId: state.nextId + 1,
  };
}

function completeNote(state: SubAgentNotesState, content: unknown): SubAgentNotesState {
  const progress = resultProgress(content);
  if (progress) return applyProgress(state, {
    invocation_id: progress.invocationId,
    agent: progress.agent,
    session_id: progress.sessionId,
    status: progress.status,
    elapsed_ms: progress.elapsedMs,
    usage: {
      input_tokens: progress.inputTokens,
      output_tokens: progress.outputTokens,
      cached_tokens: progress.cachedTokens,
    },
    tool_call_count: progress.toolCallCount,
  });
  const marker = resultMarker(content);
  if (!marker) return state;

  const index = state.notes.findLastIndex((note) => (
    isMatchingRunningNote(note, marker)
  ));
  if (index < 0) return state;

  return {
    ...state,
    notes: state.notes.map((note, noteIndex) => (
      noteIndex === index ? freezeNote(note, 'done') : note
    )),
  };
}

function freezeNote(note: SubAgentNote, status: Exclude<SubAgentNoteStatus, 'running'>): SubAgentNote {
  const localElapsed = note.status === 'running'
    ? Math.max(0, Date.now() - note.updatedAtMs)
    : 0;
  return {
    ...note,
    status,
    elapsedMs: note.elapsedMs + localElapsed,
    updatedAtMs: Date.now(),
  };
}

function failRunningNote(note: SubAgentNote): SubAgentNote {
  return note.status === 'running' ? freezeNote(note, 'failed') : note;
}

function failUnresolvedNotes(state: SubAgentNotesState): SubAgentNotesState {
  const hasRunningNotes = state.notes.some((note) => note.status === 'running');
  if (!hasRunningNotes && state.latestParentMessageId === null) return state;
  return {
    ...state,
    latestParentMessageId: null,
    notes: hasRunningNotes
      ? state.notes.map(failRunningNote)
      : state.notes,
  };
}

function resetNotes(): SubAgentNotesState {
  return INITIAL_SUB_AGENT_NOTES_STATE;
}

function startRun(state: SubAgentNotesState): SubAgentNotesState {
  if (state.latestParentMessageId === null) return state;
  return { ...state, latestParentMessageId: null };
}

function startToolCall(
  state: SubAgentNotesState,
  event: EventRecord,
): SubAgentNotesState {
  const parentMessageId = nonBlankString(
    event.parentMessageId ?? event.parent_message_id,
  );
  if (!parentMessageId) return state;
  if (parentMessageId === state.latestParentMessageId) return state;
  return { ...state, latestParentMessageId: parentMessageId };
}

function startFromCustomEvent(
  state: SubAgentNotesState,
  event: EventRecord,
): SubAgentNotesState {
  if (event.name === 'sub_agent_started') return startNote(state, event.value);
  if (event.name === 'sub_agent_progress') return applyProgress(state, event.value);
  return state;
}

function completeFromToolResult(
  state: SubAgentNotesState,
  event: EventRecord,
): SubAgentNotesState {
  return completeNote(state, event.content);
}

function restoreSnapshot(
  _state: SubAgentNotesState,
  event: EventRecord,
): SubAgentNotesState {
  const notes = notesFromSnapshot(event.messages);
  return {
    notes,
    latestParentMessageId: null,
    nextId: notes.length,
  };
}

const EVENT_REDUCERS: Record<string, EventReducer> = {
  RESET: resetNotes,
  RUN_STARTED: startRun,
  TOOL_CALL_START: startToolCall,
  CUSTOM: startFromCustomEvent,
  TOOL_CALL_RESULT: completeFromToolResult,
  MESSAGES_SNAPSHOT: restoreSnapshot,
  RUN_FINISHED: failUnresolvedNotes,
  RUN_ERROR: failUnresolvedNotes,
};

export function reduceSubAgentNotes(
  state: SubAgentNotesState,
  event: unknown,
): SubAgentNotesState {
  const value = record(event);
  if (!value) return state;
  const reducer = EVENT_REDUCERS[eventType(event) ?? ''];
  return reducer ? reducer(state, value) : state;
}
