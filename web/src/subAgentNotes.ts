export type SubAgentNoteStatus = 'running' | 'done' | 'failed';

export interface SubAgentNote {
  id: string;
  agent: string;
  sessionId: string;
  parentMessageId: string;
  status: SubAgentNoteStatus;
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
}

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
): SubAgentIdentity | undefined {
  const agent = nonBlankString(agentValue);
  if (!agent) return undefined;
  const sessionId = nonBlankString(sessionIdValue);
  if (!sessionId) return undefined;
  return { agent, sessionId };
}

function subAgentMarker(value: unknown): SubAgentIdentity | undefined {
  const marker = record(record(value)?.sub_agent);
  return subAgentIdentity(marker?.agent, marker?.session_id);
}

function resultMarker(content: unknown): SubAgentIdentity | undefined {
  if (typeof content === 'string') {
    try {
      return subAgentMarker(JSON.parse(content));
    } catch {
      return undefined;
    }
  }
  return subAgentMarker(content);
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
  if (nonBlankString(message.role) !== 'tool') return undefined;
  const callId = toolCallId(message);
  if (!callId) return undefined;
  const parentMessageId = parentByToolCall.get(callId);
  if (!parentMessageId) return undefined;
  const marker = resultMarker(message.content);
  if (!marker) return undefined;
  return {
    id: `snapshot:${parentMessageId}:${callId}`,
    agent: marker.agent,
    sessionId: marker.sessionId,
    parentMessageId,
    status: 'done',
  };
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
  return subAgentIdentity(marker?.agent, marker?.session_id);
}

function sameIdentity(note: SubAgentNote, identity: SubAgentIdentity): boolean {
  if (note.agent !== identity.agent) return false;
  return note.sessionId === identity.sessionId;
}

function isDuplicateRunningNote(
  note: SubAgentNote,
  identity: SubAgentIdentity,
  parentMessageId: string,
): boolean {
  if (note.status !== 'running') return false;
  if (note.parentMessageId !== parentMessageId) return false;
  return sameIdentity(note, identity);
}

function hasDuplicateRunningNote(
  state: SubAgentNotesState,
  identity: SubAgentIdentity,
  parentMessageId: string,
): boolean {
  return state.notes.some((note) => (
    isDuplicateRunningNote(note, identity, parentMessageId)
  ));
}

function runningNote(
  state: SubAgentNotesState,
  identity: SubAgentIdentity,
  parentMessageId: string,
): SubAgentNote {
  return {
    id: `live:${state.nextId}`,
    agent: identity.agent,
    sessionId: identity.sessionId,
    parentMessageId,
    status: 'running',
  };
}

function startNote(state: SubAgentNotesState, value: unknown): SubAgentNotesState {
  const identity = startedIdentity(value);
  if (!identity) return state;
  const parentMessageId = state.latestParentMessageId;
  if (!parentMessageId) return state;
  if (hasDuplicateRunningNote(state, identity, parentMessageId)) return state;

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
  return sameIdentity(note, identity);
}

function completeNote(state: SubAgentNotesState, content: unknown): SubAgentNotesState {
  const marker = resultMarker(content);
  if (!marker) return state;

  const index = state.notes.findLastIndex((note) => (
    isMatchingRunningNote(note, marker)
  ));
  if (index < 0) return state;

  return {
    ...state,
    notes: state.notes.map((note, noteIndex) => (
      noteIndex === index ? { ...note, status: 'done' } : note
    )),
  };
}

function failRunningNote(note: SubAgentNote): SubAgentNote {
  return note.status === 'running' ? { ...note, status: 'failed' } : note;
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
  if (event.name !== 'sub_agent_started') return state;
  return startNote(state, event.value);
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
