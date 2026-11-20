import React, { useState, useCallback, useId } from 'react';
import type { A2uiSurface, A2uiSurfaceResolution } from './surfaceState';
import { humanizeRejectionOutcome } from './surfaceState';
import { useA2uiSurfaces } from './A2uiSurfacesContext';
import { A2UIForm } from './A2UIForm';
import { buildData, defaultValue, inputFields } from './buildData';
import { submitSurfaceAction, SurfaceActionError } from '../api';

export interface A2uiSurfaceCardProps {
  surface: A2uiSurface;
  agentName?: string;
  sessionId?: string;
  onSubmitAction?: (
    actionName: string,
    data: Record<string, unknown>,
    note?: string
  ) => Promise<void>;
}

export interface SurfaceErrorBoundaryProps {
  children: React.ReactNode;
}

export interface SurfaceErrorBoundaryState {
  hasError: boolean;
  error?: Error;
}

export class SurfaceErrorBoundary extends React.Component<
  SurfaceErrorBoundaryProps,
  SurfaceErrorBoundaryState
> {
  constructor(props: SurfaceErrorBoundaryProps) {
    super(props);
    this.state = { hasError: false };
  }

  static getDerivedStateFromError(error: Error): SurfaceErrorBoundaryState {
    return { hasError: true, error };
  }

  componentDidCatch(_error: Error, _errorInfo: React.ErrorInfo) {
    // Isolate component graph render errors to this surface card
  }

  render() {
    if (this.state.hasError) {
      return (
        <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
          Unable to render surface: {this.state.error?.message || 'Component render error'}
        </div>
      );
    }
    return this.props.children;
  }
}

export const A2uiSurfaceCard: React.FC<A2uiSurfaceCardProps> = ({
  surface,
  agentName: propsAgentName,
  sessionId: propsSessionId,
  onSubmitAction,
}) => {
  const context = useA2uiSurfaces();
  const agentName = propsAgentName ?? context.agentName;
  const sessionId = propsSessionId ?? context.sessionId;

  const [submitting, setSubmitting] = useState(false);
  const [submittingAction, setSubmittingAction] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState('');
  const [localResolution, setLocalResolution] = useState<A2uiSurfaceResolution | null>(null);
  const [isConflicting, setIsConflicting] = useState(false);

  const resolution = surface.action ?? localResolution;
  const isResolved = Boolean(resolution);
  const isFormDisabled = submitting || isResolved || isConflicting;

  const initFormValues = useCallback((): Record<string, unknown> => {
    try {
      if (surface.payload) {
        const initial: Record<string, unknown> = {};
        for (const field of inputFields(surface.payload)) {
          initial[field.key] = defaultValue(
            surface.payload,
            field.path,
            field.kind === 'ChoicePicker'
          );
        }
        return initial;
      }
    } catch {
      return {};
    }
    return {};
  }, [surface.payload]);

  const [formValues, setFormValues] = useState<Record<string, unknown>>(initFormValues);
  const noteInputId = useId();

  const handleAction = async (actionName: string) => {
    setError(null);
    setSubmitting(true);
    setSubmittingAction(actionName);
    try {
      const trimmedNote = note.trim();
      let data: Record<string, unknown> = {};
      if (surface.payload) {
        data = buildData(surface.payload, actionName, formValues);
      }

      if (onSubmitAction) {
        await onSubmitAction(actionName, data, trimmedNote || undefined);
        const resObj: A2uiSurfaceResolution = {
          action: actionName,
          noteText: trimmedNote || undefined,
        };
        setLocalResolution(resObj);
        context.updateSurfaceResolution(surface.surfaceId, surface.publishSeq, resObj);
      } else {
        const res = await submitSurfaceAction(agentName, sessionId, {
          surfaceId: surface.surfaceId,
          publishSeq: surface.publishSeq,
          response: {
            action: actionName,
            data,
          },
          userMessage: trimmedNote.length > 0 ? trimmedNote : undefined,
        });
        if (res.outcome === 'applied' || res.outcome === 'already_applied') {
          const resObj: A2uiSurfaceResolution = {
            action: actionName,
            noteText: trimmedNote || undefined,
          };
          setLocalResolution(resObj);
          context.updateSurfaceResolution(surface.surfaceId, surface.publishSeq, resObj);
        }
      }
    } catch (err: unknown) {
      let msg = 'Failed to submit surface action';
      if (err instanceof SurfaceActionError) {
        if (err.outcome === 'conflicting') {
          setIsConflicting(true);
        }
        msg = err.outcome ? humanizeRejectionOutcome(err.outcome) : err.message;
      } else if (err instanceof Error) {
        msg = err.message;
      } else {
        msg = String(err);
      }
      setError(msg);
    } finally {
      setSubmitting(false);
      setSubmittingAction(null);
    }
  };

  return (
    <div
      className={`a2ui-surface-card ${isResolved ? 'a2ui-surface-card-resolved' : ''} ${
        isConflicting ? 'a2ui-surface-card-conflicting' : ''
      } ${submitting ? 'a2ui-surface-card-submitting' : ''}`}
      data-testid={`a2ui-surface-${surface.surfaceId}-${surface.publishSeq}`}
      data-surface-id={surface.surfaceId}
      data-publish-seq={surface.publishSeq}
    >
      {isResolved && resolution && (
        <div
          className="a2ui-surface-status a2ui-surface-status-resolved"
          data-testid="a2ui-surface-resolved"
        >
          <span>
            Action applied: <strong>{resolution.action}</strong>
          </span>
          {resolution.visibility && (
            <span className="a2ui-surface-visibility"> ({resolution.visibility})</span>
          )}
          {resolution.noteText && (
            <span className="a2ui-surface-note-text"> — {resolution.noteText}</span>
          )}
        </div>
      )}

      {submitting && (
        <div
          className="a2ui-surface-status a2ui-surface-status-pending"
          data-testid="a2ui-surface-pending"
        >
          <span className="aui-spinner" aria-hidden="true" />
          <span>Submitting {submittingAction}...</span>
        </div>
      )}

      {surface.payload ? (
        <SurfaceErrorBoundary>
          <A2UIForm
            payload={surface.payload}
            values={formValues}
            onValueChange={(key, val) =>
              setFormValues((prev) => ({ ...prev, [key]: val }))
            }
            onAction={handleAction}
            disabled={isFormDisabled}
          />
        </SurfaceErrorBoundary>
      ) : (
        !isResolved && (
          <div role="alert" className="a2ui-surface-error">
            Unable to render surface
          </div>
        )
      )}

      {!isResolved && (
        <div className="a2ui-surface-note-input">
          <label htmlFor={noteInputId}>Optional Note:</label>
          <input
            id={noteInputId}
            type="text"
            value={note}
            onChange={(e) => setNote(e.target.value)}
            disabled={submitting || isConflicting}
            placeholder="Add note for action..."
          />
        </div>
      )}

      {error && (
        <div role="alert" className="a2ui-surface-error" data-testid="a2ui-surface-error">
          {error}
        </div>
      )}
    </div>
  );
};
