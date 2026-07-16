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
}
