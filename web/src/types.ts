export interface Agent {
  name: string;
  model: string;
  description?: string | null;
  role: string;
  [key: string]: unknown;
}

export interface SessionRef {
  session_id: string;
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
}

