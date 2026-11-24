import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi, beforeEach } from 'vitest';
import { SubAgentRow } from '../SubAgentRow';
import { SubAgentNotesContext } from '../SubAgentNotesContext';
import type { SubAgentNote } from '../subAgentNotes';
import {
  type LegacyApprovalPendingInteraction,
  type ConfirmPendingInteraction,
  type UnsupportedPendingInteraction,
  getPendingInteractionKey,
} from '../subAgentPendingInteraction';
import { reduceSubAgentNotes } from '../subAgentNotes';
import { useReducer } from 'react';

describe('SubAgentRow pending interactions', () => {
  const onOpen = vi.fn();
  const dispatch = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
  });

  function renderRow(note: SubAgentNote) {
    return render(
      <SubAgentNotesContext.Provider value={{ notes: [note], openSession: onOpen, dispatch }}>
        <SubAgentRow note={note} nowMs={Date.now()} onOpen={onOpen} />
      </SubAgentNotesContext.Provider>,
    );
  }

  const baseNote: SubAgentNote = {
    id: 'live:inv-1',
    invocationId: 'inv-1',
    toolCallId: 'call-parent',
    agent: 'researcher',
    sessionId: 'child-sess-100',
    parentMessageId: 'parent-msg',
    status: 'running',
    elapsedMs: 1000,
    inputTokens: 10,
    outputTokens: 20,
    cachedTokens: 5,
    toolCallCount: 1,
    updatedAtMs: Date.now(),
  };

  const legacyPending: LegacyApprovalPendingInteraction = {
    agent: 'researcher',
    cluster: '__local__',
    sessionId: 'child-sess-100',
    toolName: 'bash',
    message: 'Run command: rm -rf /tmp/data',
    pendingCount: 1,
    supported: true,
    type: 'legacy_approval',
    request: {
      invocation_id: 'child-inv-1',
      tool_round_seq: 1,
      request_seq: 2,
      tool_call_id: 'call-danger',
    },
  };

  const confirmPending: ConfirmPendingInteraction = {
    agent: 'researcher',
    cluster: 'remote-node',
    sessionId: 'child-sess-100',
    toolName: 'deploy',
    message: 'Deploy revision v2.0',
    pendingCount: 1,
    supported: true,
    type: 'confirm',
    interactionId: 'interaction-deploy-99',
    requestSeq: 4,
  };

  const unsupportedPending: UnsupportedPendingInteraction = {
    agent: 'researcher',
    cluster: '__local__',
    sessionId: 'child-sess-100',
    toolName: 'ask_user',
    message: 'Provide additional authentication credentials',
    pendingCount: 1,
    supported: false,
    type: 'unsupported',
    rawKind: 'interaction',
    interactionKind: 'credentials_prompt',
  };

  describe('Legacy approval interactions', () => {
    it('handles Approve on real row: calls session/hitl_decision_exact on child URL with approved: true and clears controls on applied', async () => {
      const user = userEvent.setup();
      let capturedUrl = '';
      let capturedBody: any = null;

      vi.stubGlobal(
        'fetch',
        vi.fn().mockImplementation(async (url: string, init: any) => {
          capturedUrl = url;
          capturedBody = JSON.parse(init.body);
          return {
            ok: true,
            status: 200,
            json: async () => ({
              jsonrpc: '2.0',
              id: 1,
              result: { applied: true, outcome: 'applied' },
            }),
          };
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);

      expect(screen.getByText('Run command: rm -rf /tmp/data')).toBeInTheDocument();
      const approveBtn = screen.getByRole('button', { name: 'Approve' });
      const noteInput = screen.getByPlaceholderText('Optional note');

      await user.type(noteInput, 'LGTM');
      await user.click(approveBtn);

      expect(capturedUrl).toBe('/v1/agents/researcher/sessions/child-sess-100');
      expect(capturedBody).toEqual({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/hitl_decision_exact',
        params: {
          request: legacyPending.request,
          approved: true,
          note: 'LGTM',
        },
      });

      await waitFor(() => {
        expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
      });
      expect(dispatch).toHaveBeenCalledWith({
        type: 'CLEAR_PENDING_INTERACTION',
        invocationId: note.invocationId,
        toolCallId: note.toolCallId,
        expectedPendingKey: getPendingInteractionKey(note.pendingInteraction),
      });
    });

    it('handles Deny on real row: calls session/hitl_decision_exact with approved: false', async () => {
      const user = userEvent.setup();
      let capturedBody: any = null;

      vi.stubGlobal(
        'fetch',
        vi.fn().mockImplementation(async (_url: string, init: any) => {
          capturedBody = JSON.parse(init.body);
          return {
            ok: true,
            status: 200,
            json: async () => ({
              jsonrpc: '2.0',
              id: 1,
              result: { applied: true, outcome: 'applied' },
            }),
          };
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);

      const denyBtn = screen.getByRole('button', { name: 'Deny' });
      await user.click(denyBtn);

      expect(capturedBody.params).toEqual({
        request: legacyPending.request,
        approved: false,
      });

      await waitFor(() => {
        expect(screen.queryByRole('button', { name: 'Deny' })).not.toBeInTheDocument();
      });
    });

    it('handles already_applied outcome: clears if same decision, shows answered differently elsewhere if different', async () => {
      const user = userEvent.setup();

      // Case 1: same decision (approved: true returned when approved: true was submitted)
      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: {
              applied: true,
              outcome: 'already_applied',
              approved: true,
              note: 'already approved',
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      const { unmount } = renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
      });
      expect(screen.queryByText(/answered differently elsewhere/i)).not.toBeInTheDocument();

      unmount();

      // Case 2: different decision (approved: false returned when approved: true was submitted)
      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: {
              applied: true,
              outcome: 'already_applied',
              approved: false,
              note: 'rejected by admin',
            },
          }),
        }),
      );

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(
          screen.getByText('answered differently elsewhere: denied (rejected by admin)'),
        ).toBeInTheDocument();
      });
      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    });

    it('handles conflicting outcome: shows answered differently elsewhere with recorded answer', async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: {
              applied: false,
              outcome: 'conflicting',
              approved: false,
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(screen.getByText('answered differently elsewhere: denied')).toBeInTheDocument();
      });
      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    });

    it('handles stale outcome: shows this request is no longer pending and clears controls', async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: {
              applied: false,
              outcome: 'stale',
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(screen.getByText('this request is no longer pending')).toBeInTheDocument();
      });
      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    });

    it('handles malformed / missing outcome ({ applied: true } alone): displays error and leaves pending', async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            result: {
              applied: true,
              // missing outcome
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(screen.getByRole('alert')).toBeInTheDocument();
      });
      expect(screen.getByRole('alert')).toHaveTextContent(
        'RPC call returned malformed exact hitl decision result: missing or unrecognized outcome',
      );
      // Controls must leave pending!
      expect(screen.getByRole('button', { name: 'Approve' })).toBeInTheDocument();
      expect(screen.getByRole('button', { name: 'Deny' })).toBeInTheDocument();
    });

    it('handles unknown method error: shows exact text and NO fallback legacy call ever', async () => {
      const user = userEvent.setup();
      const fetchMock = vi.fn().mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          jsonrpc: '2.0',
          id: 1,
          error: {
            code: -32601,
            message: 'Method not found',
          },
        }),
      });
      vi.stubGlobal('fetch', fetchMock);

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(screen.getByRole('alert')).toHaveTextContent(
          "this server can't answer from here; open the child session",
        );
      });

      // No fallback call was made!
      expect(fetchMock).toHaveBeenCalledTimes(1);
      // Controls leave pending!
      expect(screen.getByRole('button', { name: 'Approve' })).toBeInTheDocument();
    });
  });

  describe('Confirm interactions (middleware)', () => {
    it('handles Approve on confirm row: calls session/interaction_response targeting child URL with action: approve', async () => {
      const user = userEvent.setup();
      let capturedUrl = '';
      let capturedBody: any = null;

      vi.stubGlobal(
        'fetch',
        vi.fn().mockImplementation(async (url: string, init: any) => {
          capturedUrl = url;
          capturedBody = JSON.parse(init.body);
          return {
            ok: true,
            status: 200,
            json: async () => ({
              jsonrpc: '2.0',
              id: 1,
              result: { applied: true, outcome: 'applied' },
            }),
          };
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: confirmPending,
      };

      renderRow(note);

      expect(screen.getByText('Deploy revision v2.0')).toBeInTheDocument();
      const approveBtn = screen.getByRole('button', { name: 'Approve' });
      const noteInput = screen.getByPlaceholderText('Optional note');

      await user.type(noteInput, 'deploy approved');
      await user.click(approveBtn);

      // Confirm pending uses remote-node cluster -> researcher@remote-node
      expect(capturedUrl).toBe('/v1/agents/researcher%40remote-node/sessions/child-sess-100');
      expect(capturedBody).toEqual({
        jsonrpc: '2.0',
        id: 1,
        method: 'session/interaction_response',
        params: {
          interaction_id: 'interaction-deploy-99',
          request_seq: 4,
          response: {
            action: 'approve',
            data: {},
            note: 'deploy approved',
          },
        },
      });

      await waitFor(() => {
        expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
      });
    });

    it('handles Deny on confirm row: calls session/interaction_response with action: deny', async () => {
      const user = userEvent.setup();
      let capturedBody: any = null;

      vi.stubGlobal(
        'fetch',
        vi.fn().mockImplementation(async (_url: string, init: any) => {
          capturedBody = JSON.parse(init.body);
          return {
            ok: true,
            status: 200,
            json: async () => ({
              jsonrpc: '2.0',
              id: 1,
              result: { applied: true, outcome: 'applied' },
            }),
          };
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: confirmPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Deny' }));

      expect(capturedBody.params.response).toEqual({
        action: 'deny',
        data: {},
      });

      await waitFor(() => {
        expect(screen.queryByRole('button', { name: 'Deny' })).not.toBeInTheDocument();
      });
    });

    it('handles conflicting on confirm with same action but different note: displays recorded action without false inference', async () => {
      const user = userEvent.setup();

      // User submitted 'approve', server conflicts with already recorded 'approve' and note 'approved via cli'
      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            error: {
              code: -32004,
              message: 'interaction response rejected',
              data: {
                outcome: 'conflicting',
                response: {
                  action: 'approve',
                  data: {},
                  note: 'approved via cli',
                },
              },
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: confirmPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      // If code had inferred opposite action (deny), it would have displayed 'denied'!
      // Verifying it displays the recorded 'approve':
      await waitFor(() => {
        expect(
          screen.getByText('answered differently elsewhere: approve (approved via cli)'),
        ).toBeInTheDocument();
      });
      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    });

    it('handles conflicting on confirm without recorded response (older serve): displays fallback text without inventing answer', async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            error: {
              code: -32004,
              message: 'interaction response rejected',
              data: {
                outcome: 'conflicting',
              },
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: confirmPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(
          screen.getByText(
            'answered differently elsewhere (recorded answer unavailable; open child session)',
          ),
        ).toBeInTheDocument();
      });
      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    });

    it('handles stale on confirm: shows this request is no longer pending and clears controls', async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: '2.0',
            id: 1,
            error: {
              code: -32004,
              message: 'interaction response rejected',
              data: {
                outcome: 'stale',
              },
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: confirmPending,
      };

      renderRow(note);
      await user.click(screen.getByRole('button', { name: 'Approve' }));

      await waitFor(() => {
        expect(screen.getByText('this request is no longer pending')).toBeInTheDocument();
      });
      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
    });
  });

  describe('Unsupported interaction kinds', () => {
    it('displays message, unsupported notice, and open-session link; does not render Approve/Deny buttons', async () => {
      const user = userEvent.setup();

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: unsupportedPending,
      };

      renderRow(note);

      expect(
        screen.getByText('Provide additional authentication credentials'),
      ).toBeInTheDocument();
      expect(
        screen.getByText('This server cannot handle this interaction from here.'),
      ).toBeInTheDocument();

      const openLink = screen.getByRole('link', { name: 'Open session' });
      expect(openLink).toBeInTheDocument();
      expect(openLink).toHaveAttribute(
        'href',
        '/agents/researcher/sessions/child-sess-100',
      );

      expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument();
      expect(screen.queryByRole('button', { name: 'Deny' })).not.toBeInTheDocument();

      await user.click(openLink);
      expect(onOpen).toHaveBeenCalledWith('researcher', 'child-sess-100');
    });
  });

  describe("Malformed exact decision responses (missing boolean approved)", () => {
    it("handles already_applied outcome missing boolean approved: shows error and stays pending", async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: "2.0",
            id: 1,
            result: {
              applied: true,
              outcome: "already_applied",
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);
      await user.click(screen.getByRole("button", { name: "Approve" }));

      await waitFor(() => {
        expect(screen.getByRole("alert")).toBeInTheDocument();
      });
      expect(screen.getByRole("alert")).toHaveTextContent(
        "missing recorded approved for already_applied",
      );
      expect(screen.getByRole("button", { name: "Approve" })).toBeInTheDocument();
    });

    it("handles conflicting outcome missing boolean approved: shows error and stays pending", async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: "2.0",
            id: 1,
            result: {
              applied: false,
              outcome: "conflicting",
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      renderRow(note);
      await user.click(screen.getByRole("button", { name: "Approve" }));

      await waitFor(() => {
        expect(screen.getByRole("alert")).toBeInTheDocument();
      });
      expect(screen.getByRole("alert")).toHaveTextContent(
        "missing recorded approved for conflicting",
      );
      expect(screen.getByRole("button", { name: "Approve" })).toBeInTheDocument();
    });
  });

  describe("Differing row and authoritative summary identity/cluster", () => {
    it("uses authoritative summary agent, cluster, and session for answer targets and links", async () => {
      const user = userEvent.setup();
      let capturedUrl = "";
      vi.stubGlobal(
        "fetch",
        vi.fn().mockImplementation(async (url: string) => {
          capturedUrl = url;
          return {
            ok: true,
            status: 200,
            json: async () => ({
              jsonrpc: "2.0",
              id: 1,
              result: { applied: true, outcome: "applied" },
            }),
          };
        }),
      );

      const differingPending: LegacyApprovalPendingInteraction = {
        agent: "child-executor",
        cluster: "cloud-west",
        sessionId: "child-session-99",
        toolName: "bash",
        message: "Execute remote migration",
        pendingCount: 1,
        supported: true,
        type: "legacy_approval",
        request: {
          invocation_id: "child-inv-99",
          tool_round_seq: 1,
          request_seq: 2,
          tool_call_id: "call-mig",
        },
      };

      const note: SubAgentNote = {
        ...baseNote,
        agent: "parent-proxy",
        sessionId: "parent-session-1",
        pendingInteraction: differingPending,
      };

      renderRow(note);

      // Row open button should target child-executor@cloud-west / child-session-99
      const rowOpenBtn = screen.getByRole("link", {
        name: /Open child-executor sub-agent session child-session-99/i,
      });
      expect(rowOpenBtn).toHaveAttribute(
        "href",
        "/agents/child-executor%40cloud-west/sessions/child-session-99",
      );

      await user.click(rowOpenBtn);
      expect(onOpen).toHaveBeenCalledWith("child-executor@cloud-west", "child-session-99");

      // Approving should send exact RPC to /v1/agents/child-executor%40cloud-west/sessions/child-session-99
      await user.click(screen.getByRole("button", { name: "Approve" }));
      expect(capturedUrl).toBe("/v1/agents/child-executor%40cloud-west/sessions/child-session-99");
    });

    it("uses authoritative summary for unsupported interaction links when differing from row", async () => {
      const user = userEvent.setup();

      const differingUnsupported: UnsupportedPendingInteraction = {
        agent: "child-worker",
        cluster: "k8s-cluster",
        sessionId: "child-sess-44",
        toolName: "oauth",
        message: "Complete browser authorization",
        pendingCount: 1,
        supported: false,
        type: "unsupported",
        rawKind: "interaction",
        interactionKind: "oauth_prompt",
      };

      const note: SubAgentNote = {
        ...baseNote,
        agent: "local-parent",
        sessionId: "local-sess-0",
        pendingInteraction: differingUnsupported,
      };

      renderRow(note);

      const openLink = screen.getByRole("link", { name: "Open session" });
      expect(openLink).toHaveAttribute(
        "href",
        "/agents/child-worker%40k8s-cluster/sessions/child-sess-44",
      );

      await user.click(openLink);
      expect(onOpen).toHaveBeenCalledWith("child-worker@k8s-cluster", "child-sess-44");
    });
  });

  it('shows a stale request again when new progress advertises the same exact generation', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      ok: true, status: 200,
      json: async () => ({ result: { applied: false, outcome: 'stale' } }),
    }));
    const note = { ...baseNote, pendingInteraction: legacyPending, progressSeq: 1 };
    const view = renderRow(note);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Deny' }));
    await waitFor(() => expect(screen.queryByRole('button', { name: 'Approve' })).not.toBeInTheDocument());
    expect(screen.getByText('this request is no longer pending')).toBeInTheDocument();
    const refreshed = { ...note, progressSeq: 2 };
    view.rerender(
      <SubAgentNotesContext.Provider value={{ notes: [refreshed], openSession: onOpen, dispatch }}>
        <SubAgentRow note={refreshed} nowMs={Date.now()} onOpen={onOpen} />
      </SubAgentNotesContext.Provider>,
    );
    expect(screen.getByRole('button', { name: 'Approve' })).toBeEnabled();
    expect(screen.queryByText('this request is no longer pending')).not.toBeInTheDocument();
  });

  describe("Real reducer integration (provider + reduceSubAgentNotes)", () => {
    function RealReducerHarness({
      initialNote,
      onOpenHandler,
    }: {
      initialNote: SubAgentNote;
      onOpenHandler: (agent: string, sessionId: string) => void;
    }) {
      const [state, dispatchAction] = useReducer(reduceSubAgentNotes, {
        notes: [initialNote],
        parentByToolCall: new Map([[initialNote.toolCallId!, initialNote.parentMessageId!]]),
        latestParentMessageId: initialNote.parentMessageId,
      });
      return (
        <SubAgentNotesContext.Provider
          value={{ notes: state.notes, openSession: onOpenHandler, dispatch: dispatchAction }}
        >
          <button
            type="button"
            data-testid="advance-progress"
            onClick={() => {
              dispatchAction({
                type: "CUSTOM",
                name: "sub_agent_progress",
                value: {
                  invocation_id: initialNote.invocationId,
                  tool_call_id: initialNote.toolCallId,
                  agent: initialNote.agent,
                  session_id: initialNote.sessionId,
                  status: "running",
                  elapsed_ms: 2000,
                  usage: { input_tokens: 20, output_tokens: 30, cached_tokens: 5 },
                  tool_call_count: 2,
                },
              });
            }}
          >
            Advance Progress
          </button>
          <button
            type="button"
            data-testid="advance-newer-pending"
            onClick={() => {
              dispatchAction({
                type: "CUSTOM",
                name: "sub_agent_progress",
                value: {
                  invocation_id: initialNote.invocationId,
                  tool_call_id: initialNote.toolCallId,
                  agent: initialNote.agent,
                  session_id: initialNote.sessionId,
                  status: "running",
                  elapsed_ms: 2500,
                  usage: { input_tokens: 25, output_tokens: 35, cached_tokens: 5 },
                  tool_call_count: 2,
                  pending_interaction: {
                    agent: initialNote.agent,
                    cluster: "__local__",
                    session_id: initialNote.sessionId,
                    tool_name: "bash",
                    message: "Newer tool call: rm -rf /other",
                    pending_count: 1,
                    kind: "legacy_approval",
                    request: {
                      invocation_id: "child-inv-1",
                      tool_round_seq: 2,
                      request_seq: 3,
                      tool_call_id: "call-newer",
                    },
                  },
                },
              });
            }}
          >
            Advance Newer Pending
          </button>
          {state.notes[0] && (
            <SubAgentRow note={state.notes[0]} nowMs={10000} onOpen={onOpenHandler} />
          )}
        </SubAgentNotesContext.Provider>
      );
    }

    it("preserves stale notice after successful real reducer clear, and resets on next progress", async () => {
      const user = userEvent.setup();

      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValueOnce({
          ok: true,
          status: 200,
          json: async () => ({
            jsonrpc: "2.0",
            id: 1,
            result: {
              applied: false,
              outcome: "stale",
            },
          }),
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      render(<RealReducerHarness initialNote={note} onOpenHandler={onOpen} />);

      expect(screen.getByRole("button", { name: "Deny" })).toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Deny" }));

      // Wait for reducer clear to remove Approve/Deny buttons
      await waitFor(() => {
        expect(screen.queryByRole("button", { name: "Deny" })).not.toBeInTheDocument();
      });

      // Stale notice MUST remain visible after the reducer cleared pendingInteraction!
      expect(screen.getByText("this request is no longer pending")).toBeInTheDocument();

      // Now next progress arrives via real reducer:
      await user.click(screen.getByTestId("advance-progress"));

      // The stale notice is now reset on next progress!
      await waitFor(() => {
        expect(screen.queryByText("this request is no longer pending")).not.toBeInTheDocument();
      });
    });

    it.each([
      { applied: true, outcome: 'applied' },
      { applied: false, outcome: 'stale' },
      { applied: false, outcome: 'conflicting', approved: false },
    ])("keeps newer generation pending after late $outcome response", async (result) => {
      const user = userEvent.setup();

      let resolveDeferredFetch!: () => void;
      const deferredPromise = new Promise<void>((resolve) => { resolveDeferredFetch = resolve; });

      vi.stubGlobal(
        "fetch",
        vi.fn().mockImplementationOnce(async () => {
          await deferredPromise;
          return {
            ok: true,
            status: 200,
            json: async () => ({
              jsonrpc: "2.0",
              id: 1,
              result,
            }),
          };
        }),
      );

      const note: SubAgentNote = {
        ...baseNote,
        pendingInteraction: legacyPending,
      };

      render(<RealReducerHarness initialNote={note} onOpenHandler={onOpen} />);

      // User clicks Approve on older pending generation (request_seq: 2)
      await user.click(screen.getByRole("button", { name: "Approve" }));

      // While fetch is still deferred / in-flight, new progress arrives with request_seq: 3!
      await user.click(screen.getByTestId("advance-newer-pending"));

      // Newer interaction message is displayed:
      expect(screen.getByText("Newer tool call: rm -rf /other")).toBeInTheDocument();

      // Now resolve the old deferred fetch:
      resolveDeferredFetch();

      // Enabled controls prove the deferred handler finished, rather than
      // passing this assertion before its response was processed.
      await waitFor(() => {
        expect(screen.getByText("Newer tool call: rm -rf /other")).toBeInTheDocument();
        expect(screen.getByRole("button", { name: "Approve" })).toBeEnabled();
      });
      expect(screen.queryByText('this request is no longer pending')).not.toBeInTheDocument();
      expect(screen.queryByText(/answered differently elsewhere/)).not.toBeInTheDocument();
    });
  });
});
