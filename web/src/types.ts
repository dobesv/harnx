import type { CompactionMetadata } from './compactionMetadata';

export interface Agent {
  name: string;
  model: string;
  description?: string | null;
  role: string;
  [key: string]: unknown;
}

export interface SessionRef {
  session_id: string;
  user_id?: string | null;
  title?: string | null;
  repository?: string | null;
  branch?: string | null;
  updated_at?: string | number | null;
  unread?: boolean;
  [key: string]: unknown;
}

export interface HistoryMessage {
  id: string;
  role: string;
  content: string;
  [key: string]: unknown;
}

export interface AgentDetail {
  name: string;
  description: string | null;
  sessions: SessionRef[];
}

export interface PaginatedSessions {
  sessions: SessionRef[];
  next_cursor: string | null;
}

export interface JsonRpcResponse<T = unknown> {
  jsonrpc: string;
  id: number | string;
  result?: T;
  error?: {
    code: number;
    message?: string;
  };
}

// What `session/cancel` answered: the session log either took a new `Cancel`,
// already had one for this turn, or had no turn to stop.
export interface CancelResult {
  outcome: 'idle' | 'accepted' | 'already_interrupted';
  cancel_seq?: number;
}

export interface PromptResult {
  status: 'accepted' | 'enqueued';
  run_id: string;
  message_id: string | null;
}

// `session/get`'s `state`. A stalled turn has no worker running it: either the
// prompt waited past the lease-acquisition timeout without a claim
// (`unclaimed`), or the worker that claimed it lost its lease
// (`worker_lost`). A worker that claims the prompt later turns the state back
// to `running` on the next read.
export interface StalledTurnState {
  status: 'stalled';
  reason: 'unclaimed' | 'worker_lost';
  prompt_seq: number;
}

export type SessionState =
  | { status: 'idle' }
  | { status: 'running'; run_id?: string; started_at?: string }
  | StalledTurnState
  | { status: 'interrupting' }
  | { status: 'interrupted'; cancel_seq: number }
  | { status: 'awaiting_approval'; pending_interrupts: unknown };

// How the most recent turn that reached a terminator ended. A turn that
// stalled wrote no terminator, so it has no `last_turn` of its own —
// `SessionState` with status `stalled` reports it instead.
export interface LastTurn {
  outcome: 'completed' | 'error' | 'timed_out' | 'interrupted';
  seq: number;
  ended_at?: string;
  message?: string;
  deadline?: string;
  requested_by?: string;
}

export interface SessionInfo {
  state?: SessionState;
  last_turn?: LastTurn;
  canPrompt?: boolean;
  canCancel?: boolean;
  history_snapshot?: HistoryMessage[];
  compaction?: CompactionMetadata;
  history_warnings?: unknown[];
  capabilities?: Record<string, unknown>;
  [key: string]: unknown;
}

export interface SessionHistoryResult {
  history_snapshot: HistoryMessage[];
  compaction?: CompactionMetadata;
  next_cursor: string | null;
  prev_cursor: string | null;
  earlier_compaction: number | null;
}

export type SessionHistoryParams =
  | { before_compaction: number; limit?: number }
  | { from_seq: number; to_seq: number; limit?: number }
  | { cursor: string; limit?: number };
