import { createContext } from 'react';

/** Hydrated HITL pending approval from durable log. */
export interface HydratedPendingApproval {
  toolCallId: string;
  summary: string;
}

export const PendingContext = createContext<{
  statusText: string | null;
  setStatusText: (t: string | null) => void;
  errorText: string | null;
  setErrorText: (t: string | null) => void;
  /** Hydrated pending approvals from hitl_pending_approval CUSTOM events. */
  hydratedApprovals: HydratedPendingApproval[];
  /** Add a hydrated approval (from hitl_pending_approval CUSTOM event). */
  addHydratedApproval: (approval: HydratedPendingApproval) => void;
  /** Clear hydrated approvals (e.g., when session changes). */
  clearHydratedApprovals: () => void;
  removeHydratedApproval: (toolCallId: string) => void;
}>({ 
  statusText: null,
  setStatusText: () => {},
  errorText: null,
  setErrorText: () => {},
  hydratedApprovals: [],
  addHydratedApproval: () => {},
  clearHydratedApprovals: () => {},
  removeHydratedApproval: () => {},
});
