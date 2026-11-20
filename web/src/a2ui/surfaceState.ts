import type { A2uiSurface, A2uiSurfaceEvent, A2uiSurfaceResolution } from './types';

export type { A2uiSurface, A2uiSurfaceEvent, A2uiSurfaceResolution };

/**
 * State map for all non-modal A2UI surfaces, keyed by `${surfaceId}:${publishSeq}`.
 */
export type A2uiSurfacesState = Map<string, A2uiSurface>;

/**
 * Unique identity key for a surface generation per T5.1 Part A.
 */
export function surfaceKey(surfaceId: string, publishSeq: number): string {
  return `${surfaceId}:${publishSeq}`;
}

export type A2uiSurfacesAction =
  | { type: 'SURFACE_EVENT'; event: A2uiSurfaceEvent }
  | {
      type: 'SURFACE_RESOLVED';
      surfaceId: string;
      publishSeq: number;
      resolution: A2uiSurfaceResolution;
    }
  | { type: 'RESET' };

/**
 * Reducer for non-modal A2UI surfaces state.
 * Deduplicates by `${surfaceId}:${publishSeq}`.
 * Preserves resolution status across duplicate or later publication events.
 */
export function reduceSurfaces(
  state: A2uiSurfacesState,
  action: A2uiSurfacesAction
): A2uiSurfacesState {
  switch (action.type) {
    case 'RESET':
      return new Map();

    case 'SURFACE_EVENT': {
      const { event } = action;
      const key = surfaceKey(event.surfaceId, event.publishSeq);
      const existing = state.get(key);
      const next = new Map(state);

      if (existing) {
        // Retain existing resolution if event doesn't supply a new one,
        // or update if event carries a resolution.
        const resolvedAction =
          event.action !== undefined && event.action !== null
            ? event.action
            : existing.action;

        next.set(key, {
          surfaceId: event.surfaceId,
          publishSeq: event.publishSeq,
          toolCallId: event.toolCallId ?? existing.toolCallId,
          payload: event.payload !== undefined ? event.payload : existing.payload,
          action: resolvedAction,
        });
      } else {
        next.set(key, {
          surfaceId: event.surfaceId,
          publishSeq: event.publishSeq,
          toolCallId: event.toolCallId,
          payload: event.payload,
          action: event.action ?? null,
        });
      }

      return next;
    }

    case 'SURFACE_RESOLVED': {
      const key = surfaceKey(action.surfaceId, action.publishSeq);
      const existing = state.get(key);
      const next = new Map(state);

      if (existing) {
        next.set(key, {
          ...existing,
          action: action.resolution,
        });
      } else {
        next.set(key, {
          surfaceId: action.surfaceId,
          publishSeq: action.publishSeq,
          action: action.resolution,
        });
      }

      return next;
    }

    default:
      return state;
  }
}

/**
 * Maps raw machine rejection outcome tokens to human-readable explanations.
 */
export function humanizeRejectionOutcome(outcome: string): string {
  switch (outcome) {
    case 'conflicting':
      return 'This surface was already resolved with a different action or input.';
    case 'unknown_surface':
      return 'This surface is no longer available or was not found.';
    case 'undeclared_action':
      return 'The requested action is not valid for this surface.';
    case 'invalid_visibility':
      return 'The requested action has an invalid visibility setting.';
    case 'invalid_tool':
      return 'The tool associated with this surface is invalid.';
    case 'invalid_data':
      return 'The submitted form data is invalid.';
    case 'unauthorized_tool':
      return 'You do not have permission to execute this tool action.';
    case 'tool_failed':
      return 'The action failed during execution.';
    case 'lease_lost':
      return 'Session ownership changed while processing this action. Please retry.';
    default:
      return `Action rejected: ${outcome.replace(/_/g, ' ')}.`;
  }
}
