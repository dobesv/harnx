import { describe, expect, it } from 'vitest';
import {
  parseSubAgentPendingInteraction,
  resolveChildAgentAddress,
  getPendingInteractionKey,
} from '../subAgentPendingInteraction';

describe('subAgentPendingInteraction', () => {
  describe('resolveChildAgentAddress', () => {
    it('returns bare agent when cluster is __local__, omitted, or empty', () => {
      expect(resolveChildAgentAddress('coder')).toBe('coder');
      expect(resolveChildAgentAddress('coder', null)).toBe('coder');
      expect(resolveChildAgentAddress('coder', undefined)).toBe('coder');
      expect(resolveChildAgentAddress('coder', '__local__')).toBe('coder');
      expect(resolveChildAgentAddress('coder', '')).toBe('coder');
    });

    it('returns agent@cluster for remote cluster names', () => {
      expect(resolveChildAgentAddress('coder', 'prod-east')).toBe('coder@prod-east');
      expect(resolveChildAgentAddress('pantheon/atlas', 'worker-2')).toBe('pantheon/atlas@worker-2');
    });
  });

  describe('parseSubAgentPendingInteraction', () => {
    it('returns undefined for non-objects or empty/null input (backward compatibility)', () => {
      expect(parseSubAgentPendingInteraction(undefined)).toBeUndefined();
      expect(parseSubAgentPendingInteraction(null)).toBeUndefined();
      expect(parseSubAgentPendingInteraction('invalid')).toBeUndefined();
      expect(parseSubAgentPendingInteraction(123)).toBeUndefined();
      expect(parseSubAgentPendingInteraction({})).toBeUndefined();
    });

    it('returns undefined when missing agent or session_id', () => {
      expect(
        parseSubAgentPendingInteraction({
          session_id: 'session-1',
          kind: 'legacy_approval',
          request: {
            invocation_id: 'inv-1',
            tool_round_seq: 1,
            request_seq: 2,
            tool_call_id: 'call-1',
          },
        }),
      ).toBeUndefined();

      expect(
        parseSubAgentPendingInteraction({
          agent: 'coder',
          kind: 'legacy_approval',
          request: {
            invocation_id: 'inv-1',
            tool_round_seq: 1,
            request_seq: 2,
            tool_call_id: 'call-1',
          },
        }),
      ).toBeUndefined();
    });

    it('parses valid legacy_approval interaction with exact wire fields', () => {
      const parsed = parseSubAgentPendingInteraction({
        agent: 'coder',
        cluster: 'worker-1',
        session_id: 'sess-123',
        tool_name: 'bash',
        message: 'Execute command: rm -rf',
        pending_count: 2,
        kind: 'legacy_approval',
        request: {
          invocation_id: 'inv-1',
          tool_round_seq: 1,
          request_seq: 2,
          tool_call_id: 'call-99',
        },
      });

      expect(parsed).toEqual({
        agent: 'coder',
        cluster: 'worker-1',
        sessionId: 'sess-123',
        toolName: 'bash',
        message: 'Execute command: rm -rf',
        pendingCount: 2,
        supported: true,
        type: 'legacy_approval',
        request: {
          invocation_id: 'inv-1',
          tool_round_seq: 1,
          request_seq: 2,
          tool_call_id: 'call-99',
        },
      });
    });

    it('returns undefined if legacy_approval request is missing or malformed', () => {
      expect(
        parseSubAgentPendingInteraction({
          agent: 'coder',
          session_id: 'sess-1',
          kind: 'legacy_approval',
          request: null,
        }),
      ).toBeUndefined();

      expect(
        parseSubAgentPendingInteraction({
          agent: 'coder',
          session_id: 'sess-1',
          kind: 'legacy_approval',
          request: {
            invocation_id: 'inv-1',
            // missing tool_round_seq
            request_seq: 2,
            tool_call_id: 'call-1',
          },
        }),
      ).toBeUndefined();

      expect(
        parseSubAgentPendingInteraction({
          agent: 'coder',
          session_id: 'sess-1',
          kind: 'legacy_approval',
          request: {
            invocation_id: '',
            tool_round_seq: 1,
            request_seq: 2,
            tool_call_id: 'call-1',
          },
        }),
      ).toBeUndefined();
    });

    it('parses valid confirm interaction (kind: interaction, interaction_kind: confirm)', () => {
      const parsed = parseSubAgentPendingInteraction({
        agent: 'reviewer',
        cluster: '__local__',
        session_id: 'sess-child',
        tool_name: 'git_push',
        message: 'Confirm push to origin/main',
        kind: 'interaction',
        interaction_id: 'act-55',
        request_seq: 3,
        interaction_kind: 'confirm',
      });

      expect(parsed).toEqual({
        agent: 'reviewer',
        cluster: '__local__',
        sessionId: 'sess-child',
        toolName: 'git_push',
        message: 'Confirm push to origin/main',
        pendingCount: 0,
        supported: true,
        type: 'confirm',
        interactionId: 'act-55',
        requestSeq: 3,
      });
    });

    it('treats kind: confirm as unsupported (wire is kind: interaction, interaction_kind: confirm)', () => {
      const parsed = parseSubAgentPendingInteraction({
        agent: 'reviewer',
        session_id: 'sess-child',
        kind: 'confirm',
        interaction_id: 'act-99',
        request_seq: 1,
      });

      expect(parsed).toEqual({
        agent: 'reviewer',
        cluster: '__local__',
        sessionId: 'sess-child',
        toolName: '',
        message: '',
        pendingCount: 0,
        supported: false,
        type: 'unsupported',
        rawKind: 'confirm',
        interactionKind: undefined,
      });
    });

    it('preserves raw identifier strings rather than trimming exact identity', () => {
      const parsed = parseSubAgentPendingInteraction({
        agent: ' coder-exact ',
        cluster: ' cluster-exact ',
        session_id: ' sess id exact ',
        kind: 'legacy_approval',
        request: {
          invocation_id: ' inv exact ',
          tool_round_seq: 1,
          request_seq: 2,
          tool_call_id: ' call exact ',
        },
      });

      expect(parsed?.agent).toBe(' coder-exact ');
      expect(parsed?.cluster).toBe(' cluster-exact ');
      expect(parsed?.sessionId).toBe(' sess id exact ');
      if (parsed?.type === 'legacy_approval') {
        expect(parsed.request.invocation_id).toBe(' inv exact ');
        expect(parsed.request.tool_call_id).toBe(' call exact ');
      }
    });

    it('parses unsupported interaction kinds with supported: false', () => {
      const unsupported1 = parseSubAgentPendingInteraction({
        agent: 'reviewer',
        session_id: 'sess-child',
        kind: 'interaction',
        interaction_id: 'act-prompt-1',
        request_seq: 1,
        interaction_kind: 'prompt_input',
        message: 'Please enter API key',
      });

      expect(unsupported1).toEqual({
        agent: 'reviewer',
        cluster: '__local__',
        sessionId: 'sess-child',
        toolName: '',
        message: 'Please enter API key',
        pendingCount: 0,
        supported: false,
        type: 'unsupported',
        rawKind: 'interaction',
        interactionKind: 'prompt_input',
      });

      const unsupported2 = parseSubAgentPendingInteraction({
        agent: 'reviewer',
        session_id: 'sess-child',
        kind: 'custom_approval_type',
        message: 'Custom interaction',
      });

      expect(unsupported2).toEqual({
        agent: 'reviewer',
        cluster: '__local__',
        sessionId: 'sess-child',
        toolName: '',
        message: 'Custom interaction',
        pendingCount: 0,
        supported: false,
        type: 'unsupported',
        rawKind: 'custom_approval_type',
        interactionKind: undefined,
      });
    });
  });
  describe('getPendingInteractionKey', () => {
    it('includes full child identity and complete legacy ApprovalRef', () => {
      const key = getPendingInteractionKey({
        agent: 'agent-1',
        cluster: 'cluster-a',
        sessionId: 'session-99',
        toolName: 'bash',
        message: 'test',
        pendingCount: 1,
        supported: true,
        type: 'legacy_approval',
        request: {
          invocation_id: 'inv-42',
          tool_round_seq: 3,
          request_seq: 7,
          tool_call_id: 'call-xyz',
        },
      });

      expect(key).toBe('legacy:cluster-a:agent-1:session-99:inv-42:3:7:call-xyz');
    });

    it('includes full child identity and interactionId/requestSeq for confirm', () => {
      const key = getPendingInteractionKey({
        agent: 'agent-2',
        cluster: '__local__',
        sessionId: 'session-100',
        toolName: 'deploy',
        message: 'deploy',
        pendingCount: 1,
        supported: true,
        type: 'confirm',
        interactionId: 'int-5',
        requestSeq: 2,
      });

      expect(key).toBe('confirm:__local__:agent-2:session-100:int-5:2');
    });
  });
});
