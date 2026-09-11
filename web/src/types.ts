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
  updated_at?: string | number | null;
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

export interface CancelResult {
  cancelled: boolean;
  disposition?: 'idle' | 'requested' | 'already_requested' | 'quiescing' | 'cancelled' | 'unconfirmed';
  execution_id?: string | null;
  cancellation_id?: string | null;
  requested_at?: string | null;
  unconfirmed_after_ms?: number;
}

export interface PromptResult {
  status: 'accepted' | 'enqueued';
  run_id: string;
}

export interface SessionControlState {
  execution_state?: "preparing" | "running" | "cancel_requested" | "quiescing" | "unconfirmed" | "completed" | "cancelled";
  state: { status: string; cancellation?: CancelResult };
  execution_id?: string;
  canPrompt?: boolean;
  canCancel?: boolean;
}
