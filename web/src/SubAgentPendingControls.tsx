import { useState, useContext, useEffect, useRef } from "react";
import { LinkButton } from "./LinkButton";
import type { SubAgentNote } from "./subAgentNotes";
import type { SubAgentSessionNotesProps } from "./SubAgentSessionNotes";
import { SubAgentNotesContext } from "./SubAgentNotesContext";
import {
  submitHitlDecisionExact,
  submitInteractionResponse,
  InteractionResponseError,
} from "./api";
import {
  getPendingInteractionKey,
  resolveChildAgentAddress,
} from "./subAgentPendingInteraction";

export interface SubAgentPendingControlsProps {
  note: SubAgentNote;
  isTerminal: boolean;
  childAddress: string;
  sessionHref: string;
  onOpen: SubAgentSessionNotesProps["onOpen"];
}

export function SubAgentPendingControls({
  note,
  isTerminal,
  childAddress,
  sessionHref,
  onOpen,
}: SubAgentPendingControlsProps) {
  const { dispatch } = useContext(SubAgentNotesContext);
  const [decisionNote, setDecisionNote] = useState("");
  const [isSubmittingDecision, setIsSubmittingDecision] = useState(false);
  const [decisionError, setDecisionError] = useState<string>();
  const [decisionNotice, setDecisionNotice] = useState<string>();
  const [clearedPendingKey, setClearedPendingKey] = useState<string>();

  const pending = note.pendingInteraction;
  const currentPendingKey = getPendingInteractionKey(pending);

  const [activePendingKey, setActivePendingKey] = useState(currentPendingKey);
  const progressKey = `${note.progressSeq ?? 0}:${note.updatedAtMs}`;
  const [observedProgressKey, setObservedProgressKey] = useState(progressKey);
  const latestPendingKey = useRef(currentPendingKey);
  useEffect(() => {
    latestPendingKey.current = currentPendingKey;
  }, [currentPendingKey]);

  // A local reducer clear preserves its notice. New progress is authoritative,
  // even when it advertises the same request again after a stale response.
  if (progressKey !== observedProgressKey || (currentPendingKey && currentPendingKey !== activePendingKey)) {
    setObservedProgressKey(progressKey);
    setActivePendingKey(currentPendingKey);
    setClearedPendingKey(undefined);
    setDecisionError(undefined);
    setDecisionNotice(undefined);
    setDecisionNote("");
  }

  const isPendingCleared = currentPendingKey !== undefined && currentPendingKey === clearedPendingKey;
  const showControls = Boolean(pending && !isTerminal && !isPendingCleared);

  const handleDecision = async (approved: boolean) => {
    if (!pending || !pending.supported) return;

    const targetAddress = resolveChildAgentAddress(pending.agent, pending.cluster);
    const childSession = pending.sessionId;
    const pendingKey = currentPendingKey ?? "";

    setIsSubmittingDecision(true);
    setDecisionError(undefined);
    setDecisionNotice(undefined);

    try {
      if (pending.type === "legacy_approval") {
        const result = await submitHitlDecisionExact(targetAddress, childSession, {
          request: pending.request,
          approved,
          note: decisionNote.trim() || undefined,
        });

        if (latestPendingKey.current !== pendingKey) return;
        if (result.outcome === "applied") {
          setClearedPendingKey(pendingKey);
          setDecisionNote("");
          dispatch({
            type: "CLEAR_PENDING_INTERACTION",
            invocationId: note.invocationId,
            toolCallId: note.toolCallId,
            expectedPendingKey: pendingKey,
          });
        } else if (result.outcome === "already_applied") {
          if (result.approved === approved) {
            setClearedPendingKey(pendingKey);
            setDecisionNote("");
            dispatch({
              type: "CLEAR_PENDING_INTERACTION",
              invocationId: note.invocationId,
              toolCallId: note.toolCallId,
              expectedPendingKey: pendingKey,
            });
          } else {
            const recorded = result.approved ? "approved" : "denied";
            const noteSuffix = result.note ? ` (${result.note})` : "";
            setDecisionNotice(`answered differently elsewhere: ${recorded}${noteSuffix}`);
            setClearedPendingKey(pendingKey);
          }
        } else if (result.outcome === "conflicting") {
          const recorded = result.approved ? "approved" : "denied";
          const noteSuffix = result.note ? ` (${result.note})` : "";
          setDecisionNotice(`answered differently elsewhere: ${recorded}${noteSuffix}`);
          setClearedPendingKey(pendingKey);
        } else if (result.outcome === "stale") {
          setDecisionNotice("this request is no longer pending");
          setClearedPendingKey(pendingKey);
          dispatch({
            type: "CLEAR_PENDING_INTERACTION",
            invocationId: note.invocationId,
            toolCallId: note.toolCallId,
            expectedPendingKey: pendingKey,
          });
        }
      } else if (pending.type === "confirm") {
        try {
          await submitInteractionResponse(targetAddress, childSession, {
            interactionId: pending.interactionId,
            requestSeq: pending.requestSeq,
            response: {
              action: approved ? "approve" : "deny",
              data: {},
              note: decisionNote.trim() || undefined,
            },
          });
          if (latestPendingKey.current !== pendingKey) return;
          setClearedPendingKey(pendingKey);
          setDecisionNote("");
          dispatch({
            type: "CLEAR_PENDING_INTERACTION",
            invocationId: note.invocationId,
            toolCallId: note.toolCallId,
            expectedPendingKey: pendingKey,
          });
        } catch (err: unknown) {
          if (latestPendingKey.current !== pendingKey) return;
          if (err instanceof InteractionResponseError) {
            if (err.outcome === "conflicting") {
              const recordedResponse = err.data?.response;
              if (recordedResponse?.action) {
                const noteSuffix = recordedResponse.note ? ` (${recordedResponse.note})` : "";
                setDecisionNotice(`answered differently elsewhere: ${recordedResponse.action}${noteSuffix}`);
              } else {
                setDecisionNotice(
                  "answered differently elsewhere (recorded answer unavailable; open child session)",
                );
              }
              setClearedPendingKey(pendingKey);
            } else if (err.outcome === "stale") {
              setDecisionNotice("this request is no longer pending");
              setClearedPendingKey(pendingKey);
              dispatch({
                type: "CLEAR_PENDING_INTERACTION",
                invocationId: note.invocationId,
                toolCallId: note.toolCallId,
                expectedPendingKey: pendingKey,
              });
            } else {
              setDecisionError(
                err.outcome ? `Interaction response rejected: ${err.outcome}` : err.message,
              );
            }
          } else if (err instanceof Error) {
            setDecisionError(err.message);
          } else {
            setDecisionError(String(err));
          }
        }
      }
    } catch (err: unknown) {
      if (latestPendingKey.current !== pendingKey) return;
      const msg = err instanceof Error ? err.message : String(err);
      setDecisionError(msg);
    } finally {
      setIsSubmittingDecision(false);
    }
  };

  return (
    <>
      {decisionNotice && (
        <div role="status" className="aui-sub-agent-interaction-notice">
          {decisionNotice}
        </div>
      )}
      {decisionError && (
        <div role="alert" className="aui-sub-agent-interaction-error">
          {decisionError}
        </div>
      )}
      {showControls && pending && (
        pending.supported ? (
          <div className="aui-sub-agent-interaction">
            {pending.message ? (
              <div className="aui-sub-agent-interaction-message">{pending.message}</div>
            ) : null}
            <div className="aui-sub-agent-interaction-controls">
              <input
                type="text"
                className="aui-sub-agent-interaction-note"
                placeholder="Optional note"
                value={decisionNote}
                onChange={(e) => setDecisionNote(e.target.value)}
                disabled={isSubmittingDecision}
                aria-label="Optional note"
              />
              <button
                type="button"
                className="aui-sub-agent-approve"
                onClick={() => void handleDecision(true)}
                disabled={isSubmittingDecision}
              >
                Approve
              </button>
              <button
                type="button"
                className="aui-sub-agent-deny"
                onClick={() => void handleDecision(false)}
                disabled={isSubmittingDecision}
              >
                Deny
              </button>
            </div>
          </div>
        ) : (
          <div className="aui-sub-agent-interaction aui-sub-agent-interaction-unsupported">
            {pending.message ? (
              <div className="aui-sub-agent-interaction-message">{pending.message}</div>
            ) : null}
            <div className="aui-sub-agent-unsupported">
              <span>This server cannot handle this interaction from here.</span>
              <LinkButton
                href={sessionHref}
                onNavigate={() => onOpen(childAddress, pending.sessionId)}
                className="aui-sub-agent-open-session"
              >
                Open session
              </LinkButton>
            </div>
          </div>
        )
      )}
    </>
  );
}
