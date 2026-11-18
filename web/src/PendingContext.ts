import { createContext } from 'react';
import type { PendingInteraction } from './harnxCustomEvents';

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
  /** Status message for neutral feedback (e.g., "Nothing to compact"). */
  statusMessage: string | null;
  setStatusMessage: (t: string | null) => void;
  /** Hydrated pending approvals from hitl_pending_approval CUSTOM events. */
  hydratedApprovals: HydratedPendingApproval[];
  /** Add a hydrated approval (from hitl_pending_approval CUSTOM event). */
  addHydratedApproval: (approval: HydratedPendingApproval) => void;
  /** Clear hydrated approvals (e.g., when session changes). */
  clearHydratedApprovals: () => void;
  removeHydratedApproval: (toolCallId: string) => void;
  /** Authoritative pending interaction snapshot from pending_interaction CUSTOM event. */
  pendingInteraction?: PendingInteraction | null;
  setPendingInteraction?: (
    interaction:
      | PendingInteraction
      | null
      | ((prev: PendingInteraction | null) => PendingInteraction | null)
  ) => void;
  clearPendingInteraction?: () => void;
  /** Only clear pending interaction if it still matches the given ID and sequence. */
  clearPendingInteractionIfMatch?: (interactionId: string, requestSeq: number) => void;
}>({
  statusText: null,
  setStatusText: () => {},
  errorText: null,
  setErrorText: () => {},
  statusMessage: null,
  setStatusMessage: () => {},
  hydratedApprovals: [],
  addHydratedApproval: () => {},
  clearHydratedApprovals: () => {},
  removeHydratedApproval: () => {},
  pendingInteraction: null,
  setPendingInteraction: () => {},
  clearPendingInteraction: () => {},
  clearPendingInteractionIfMatch: () => {},
});
