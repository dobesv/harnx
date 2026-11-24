import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { A2uiSurfaceCard, SurfaceErrorBoundary } from '../a2ui/A2uiSurfaceCard';
import * as A2UIFormModule from '../a2ui/A2UIForm';
import { A2uiSurfacesContext } from '../a2ui/A2uiSurfacesContext';
import { reduceSurfaces, type A2uiSurfacesState, type A2uiSurface } from '../a2ui/surfaceState';
import { submitSurfaceAction, SurfaceActionError } from '../api';
import { ToolCallCard } from '../ToolCallCard';
import { UsageContext } from '../UsageContext';

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return {
    ...actual,
    submitSurfaceAction: vi.fn(),
  };
});

vi.mock('@assistant-ui/react-markdown', () => ({
  MarkdownTextPrimitive: ({ text, children }: any) => <div>{text}{children}</div>,
}));

vi.mock('react-markdown', () => ({
  default: ({ children }: any) => <div>{children}</div>,
}));

vi.mock('react-json-view-lite', () => ({
  JsonView: ({ data }: any) => <div data-testid="json-view">{JSON.stringify(data)}</div>,
  darkStyles: {},
  defaultStyles: {},
}));

describe('Non-modal A2UI Surfaces', () => {
  beforeEach(() => {
    vi.mocked(submitSurfaceAction).mockReset();
  });

  const testPayload = {
    version: '1.0',
    createSurface: {
      surfaceId: 'surf-deploy',
      dataModel: {
        targetEnv: 'staging',
        replicas: '3',
      },
      components: [
        { id: 'root', component: 'Card', child: 'col' },
        {
          id: 'col',
          component: 'Column',
          children: ['title-text', 'env-field', 'replicas-field', 'deploy-btn', 'cancel-btn'],
        },
        {
          id: 'title-text',
          component: 'Text',
          text: 'Deploy Service',
        },
        {
          id: 'env-field',
          component: 'TextField',
          label: 'Environment',
          value: { '@path': '/targetEnv' },
        },
        {
          id: 'replicas-field',
          component: 'TextField',
          label: 'Replicas',
          value: { '@path': '/replicas' },
        },
        {
          id: 'deploy-btn',
          component: 'Button',
          child: 'deploy-label',
          variant: 'primary',
          action: {
            event: {
              name: 'deploy',
              context: {
                selectedEnv: { '@path': '/targetEnv' },
              },
            },
          },
          metadata: {
            extensions: {
              harnx: {
                visibility: 'note',
                tool: { name: 'deploy_service' },
              },
            },
          },
        },
        {
          id: 'deploy-label',
          component: 'Text',
          text: 'Deploy Now',
        },
        {
          id: 'cancel-btn',
          component: 'Button',
          child: 'cancel-label',
          action: {
            event: {
              name: 'cancel',
            },
          },
          metadata: {
            extensions: {
              harnx: {
                visibility: 'invisible',
              },
            },
          },
        },
        {
          id: 'cancel-label',
          component: 'Text',
          text: 'Cancel',
        },
      ],
    },
  };

  describe('reduceSurfaces', () => {
    it('initializes and resets state correctly', () => {
      let state: A2uiSurfacesState = new Map();
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 1,
          payload: testPayload,
        },
      });
      expect(state.size).toBe(1);
      expect(state.get('surf-1:1')).toBeDefined();

      state = reduceSurfaces(state, { type: 'RESET' });
      expect(state.size).toBe(0);
    });

    it('deduplicates publication events by surfaceId + publishSeq', () => {
      let state: A2uiSurfacesState = new Map();
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          toolCallId: 'call-1',
          payload: testPayload,
        },
      });
      expect(state.size).toBe(1);

      // Re-emitting identical event keeps single entry
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          toolCallId: 'call-1',
          payload: testPayload,
        },
      });
      expect(state.size).toBe(1);
      expect(state.get('surf-1:5')?.action).toBeNull();
    });

    it('resolves surface when resolution event arrives after publication', () => {
      let state: A2uiSurfacesState = new Map();
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          toolCallId: 'call-1',
          payload: testPayload,
        },
      });

      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          action: {
            action: 'deploy',
            visibility: 'note',
            noteText: 'deployed successfully',
          },
        },
      });

      expect(state.size).toBe(1);
      const surface = state.get('surf-1:5');
      expect(surface?.payload).toEqual(testPayload);
      expect(surface?.action).toEqual({
        action: 'deploy',
        visibility: 'note',
        noteText: 'deployed successfully',
      });
    });

    it('preserves resolution when publication arrives after resolution (out-of-order)', () => {
      let state: A2uiSurfacesState = new Map();
      // Resolution arrives first
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          action: {
            action: 'deploy',
            visibility: 'note',
          },
        },
      });

      // Publication arrives later
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          toolCallId: 'call-1',
          payload: testPayload,
        },
      });

      expect(state.size).toBe(1);
      const surface = state.get('surf-1:5');
      expect(surface?.payload).toEqual(testPayload);
      expect(surface?.action?.action).toBe('deploy');
    });

    it('SURFACE_RESOLVED action marks surface resolved', () => {
      let state: A2uiSurfacesState = new Map();
      state = reduceSurfaces(state, {
        type: 'SURFACE_EVENT',
        event: {
          surfaceId: 'surf-1',
          publishSeq: 5,
          payload: testPayload,
        },
      });

      state = reduceSurfaces(state, {
        type: 'SURFACE_RESOLVED',
        surfaceId: 'surf-1',
        publishSeq: 5,
        resolution: { action: 'cancel' },
      });

      expect(state.get('surf-1:5')?.action).toEqual({ action: 'cancel' });
    });
  });

  describe('A2uiSurfaceCard Component', () => {
    const sampleSurface: A2uiSurface = {
      surfaceId: 'surf-deploy',
      publishSeq: 3,
      toolCallId: 'call-deploy',
      payload: testPayload,
      action: null,
    };

    function renderCard(surface = sampleSurface, contextOverrides = {}) {
      const defaultContext = {
        agentName: 'agent-1',
        sessionId: 'session-1',
        surfaces: new Map([[ `${surface.surfaceId}:${surface.publishSeq}`, surface ]]),
        getSurfacesForToolCall: (id: string) => (surface.toolCallId === id ? [surface] : []),
        unattachedSurfaces: !surface.toolCallId ? [surface] : [],
        updateSurfaceResolution: vi.fn(),
        ...contextOverrides,
      };

      return {
        ...render(
          <A2uiSurfacesContext.Provider value={defaultContext}>
            <A2uiSurfaceCard surface={surface} />
          </A2uiSurfacesContext.Provider>
        ),
        context: defaultContext,
      };
    }

    it('renders form inputs, default values, and button visibility hints', () => {
      renderCard();

      expect(screen.getByText('Deploy Service')).toBeInTheDocument();
      const envInput = screen.getByLabelText('Environment') as HTMLInputElement;
      expect(envInput).toBeInTheDocument();
      expect(envInput.value).toBe('staging');

      const replicasInput = screen.getByLabelText('Replicas') as HTMLInputElement;
      expect(replicasInput.value).toBe('3');

      const deployBtn = screen.getByTestId('a2ui-btn-deploy');
      expect(deployBtn).toBeInTheDocument();
      expect(deployBtn).toHaveAttribute('data-visibility', 'note');
      expect(deployBtn).toHaveAttribute('title', 'Visibility: note');

      const cancelBtn = screen.getByTestId('a2ui-btn-cancel');
      expect(cancelBtn).toHaveAttribute('data-visibility', 'invisible');
      expect(cancelBtn).toHaveAttribute('title', 'Visibility: invisible');
    });

    it('submits action with data built per contract and userMessage note', async () => {
      vi.mocked(submitSurfaceAction).mockResolvedValueOnce({
        applied: true,
        outcome: 'applied',
      });

      const { context } = renderCard();

      const envInput = screen.getByLabelText('Environment');
      fireEvent.change(envInput, { target: { value: 'production' } });

      const noteInput = screen.getByLabelText('Optional Note:');
      fireEvent.change(noteInput, { target: { value: '  approved by team  ' } });

      const deployBtn = screen.getByTestId('a2ui-btn-deploy');
      fireEvent.click(deployBtn);

      await waitFor(() => {
        expect(submitSurfaceAction).toHaveBeenCalledTimes(1);
      });

      expect(submitSurfaceAction).toHaveBeenCalledWith('agent-1', 'session-1', {
        surfaceId: 'surf-deploy',
        publishSeq: 3,
        response: {
          action: 'deploy',
          data: {
            targetEnv: 'production',
            replicas: '3',
            selectedEnv: 'production', // resolved from context binding against updated targetEnv
          },
        },
        userMessage: 'approved by team',
      });

      await waitFor(() => {
        expect(screen.getByTestId('a2ui-surface-resolved')).toBeInTheDocument();
      });
      expect(screen.getByText(/Action applied:/)).toBeInTheDocument();
      expect(context.updateSurfaceResolution).toHaveBeenCalledWith('surf-deploy', 3, {
        action: 'deploy',
        noteText: 'approved by team',
      });
    });

    it('omits userMessage when note is empty or whitespace only', async () => {
      vi.mocked(submitSurfaceAction).mockResolvedValueOnce({
        applied: true,
        outcome: 'applied',
      });

      renderCard();

      const noteInput = screen.getByLabelText('Optional Note:');
      fireEvent.change(noteInput, { target: { value: '   ' } });

      const deployBtn = screen.getByTestId('a2ui-btn-deploy');
      fireEvent.click(deployBtn);

      await waitFor(() => {
        expect(submitSurfaceAction).toHaveBeenCalledTimes(1);
      });

      expect(submitSurfaceAction).toHaveBeenCalledWith('agent-1', 'session-1', {
        surfaceId: 'surf-deploy',
        publishSeq: 3,
        response: {
          action: 'deploy',
          data: {
            targetEnv: 'staging',
            replicas: '3',
            selectedEnv: 'staging',
          },
        },
        userMessage: undefined,
      });
    });

    it('handles already_applied outcome as success', async () => {
      vi.mocked(submitSurfaceAction).mockResolvedValueOnce({
        applied: true,
        outcome: 'already_applied',
      });

      renderCard();

      const deployBtn = screen.getByTestId('a2ui-btn-deploy');
      fireEvent.click(deployBtn);

      await waitFor(() => {
        expect(screen.getByTestId('a2ui-surface-resolved')).toBeInTheDocument();
      });
      expect(screen.getByText('deploy')).toBeInTheDocument();
    });

    it.each([
      ['unknown_surface', 'This surface is no longer available or was not found.'],
      ['undeclared_action', 'The requested action is not valid for this surface.'],
      ['invalid_visibility', 'The requested action has an invalid visibility setting.'],
      ['invalid_data', 'The submitted form data is invalid.'],
      ['unauthorized_tool', 'You do not have permission to execute this tool action.'],
      ['tool_failed', 'The action failed during execution.'],
      ['lease_lost', 'Session ownership changed while processing this action. A direct tool may have run; check the session before retrying.'],
      ['append_failed', 'The action record could not be confirmed. A direct tool may have run, and the record may already be durable; check the session before retrying.'],
      ['pending', 'The worker is still processing this action. Check the session for its outcome before trying again.'],
    ])('displays humanized rejection error (%s) without crashing, preserving form inputs', async (outcome, expectedMsg) => {
      vi.mocked(submitSurfaceAction).mockRejectedValueOnce(
        new SurfaceActionError('surface action rejected', -32004, outcome, { outcome })
      );

      renderCard();

      const envInput = screen.getByLabelText('Environment');
      fireEvent.change(envInput, { target: { value: 'custom-env' } });

      const deployBtn = screen.getByTestId('a2ui-btn-deploy');
      fireEvent.click(deployBtn);

      await waitFor(() => {
        expect(screen.getByTestId('a2ui-surface-error')).toHaveTextContent(expectedMsg);
      });

      // Form remains intact and enabled for retry
      expect((screen.getByLabelText('Environment') as HTMLInputElement).value).toBe('custom-env');
      expect(deployBtn).not.toBeDisabled();
    });

    it('displays humanized rejection error and disables form when outcome is conflicting', async () => {
      vi.mocked(submitSurfaceAction).mockRejectedValueOnce(
        new SurfaceActionError('surface action rejected', -32004, 'conflicting', { outcome: 'conflicting' })
      );

      renderCard();

      const envInput = screen.getByLabelText('Environment') as HTMLInputElement;
      fireEvent.change(envInput, { target: { value: 'custom-env' } });

      const noteInput = screen.getByLabelText('Optional Note:') as HTMLInputElement;
      fireEvent.change(noteInput, { target: { value: 'draft note' } });

      const deployBtn = screen.getByTestId('a2ui-btn-deploy');
      fireEvent.click(deployBtn);

      await waitFor(() => {
        expect(screen.getByTestId('a2ui-surface-error')).toHaveTextContent(
          'This surface was already resolved with a different action or input.'
        );
      });

      // Form preserves inputs but is disabled
      expect(envInput.value).toBe('custom-env');
      expect(envInput).toBeDisabled();
      expect(deployBtn).toBeDisabled();
      expect(noteInput).toBeDisabled();
    });

    it('renders initial resolved state as disabled when hydrated from durable log', () => {
      const resolvedSurface: A2uiSurface = {
        ...sampleSurface,
        action: {
          action: 'deploy',
          visibility: 'note',
          noteText: 'deployed on 2026-11-19',
        },
      };

      renderCard(resolvedSurface);

      expect(screen.getByTestId('a2ui-surface-resolved')).toBeInTheDocument();
      expect(screen.getByText('deploy')).toBeInTheDocument();
      expect(screen.getByText('(note)')).toBeInTheDocument();
      expect(screen.getByText(/deployed on 2026-11-19/)).toBeInTheDocument();

      // Form inputs and buttons are disabled
      expect(screen.getByLabelText('Environment')).toBeDisabled();
      expect(screen.getByLabelText('Replicas')).toBeDisabled();
      expect(screen.getByTestId('a2ui-btn-deploy')).toBeDisabled();
      expect(screen.queryByLabelText('Optional Note:')).not.toBeInTheDocument();
    });

    it('renders self-referencing surface without throwing', () => {
      const selfReferencingPayload = {
        createSurface: {
          components: [
            {
              id: 'root',
              component: 'Card',
              child: 'root',
            },
          ],
        },
      };

      const selfRefSurface: A2uiSurface = {
        surfaceId: 'surf-self-ref',
        publishSeq: 1,
        payload: selfReferencingPayload,
        action: null,
      };

      expect(() => {
        renderCard(selfRefSurface);
      }).not.toThrow();

      const errorAlert = screen.getByRole('alert');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent(/Cyclic reference detected/i);
    });

    it('renders two-node-cycle surface without throwing', () => {
      const twoNodeCyclePayload = {
        createSurface: {
          components: [
            {
              id: 'root',
              component: 'Card',
              child: 'child-col',
            },
            {
              id: 'child-col',
              component: 'Column',
              children: ['root'],
            },
          ],
        },
      };

      const cycleSurface: A2uiSurface = {
        surfaceId: 'surf-cycle-2',
        publishSeq: 1,
        payload: twoNodeCyclePayload,
        action: null,
      };

      expect(() => {
        renderCard(cycleSurface);
      }).not.toThrow();

      const errorAlert = screen.getByRole('alert');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent(/Cyclic reference detected/i);
    });

    it('renders valid deep component graph with depth between 34 and 64 successfully', () => {
      const deepComponents: Array<Record<string, unknown>> = [];
      for (let i = 0; i <= 40; i++) {
        deepComponents.push({
          id: `node-${i}`,
          component: 'Card',
          child: `node-${i + 1}`,
        });
      }
      deepComponents.push({
        id: 'node-41',
        component: 'Text',
        text: 'Deep Leaf 41',
      });

      const deepPayload = {
        createSurface: {
          components: deepComponents,
        },
      };

      const deepSurface: A2uiSurface = {
        surfaceId: 'surf-deep-valid',
        publishSeq: 1,
        payload: deepPayload,
        action: null,
      };

      expect(() => {
        renderCard(deepSurface);
      }).not.toThrow();

      expect(screen.queryByRole('alert')).not.toBeInTheDocument();
      expect(screen.getByText('Deep Leaf 41')).toBeInTheDocument();
    });

    it('renders deep component graph exceeding max depth without throwing', () => {
      const deepComponents: Array<Record<string, unknown>> = [];
      for (let i = 0; i <= 65; i++) {
        deepComponents.push({
          id: `node-${i}`,
          component: 'Card',
          child: `node-${i + 1}`,
        });
      }
      deepComponents.push({
        id: 'node-66',
        component: 'Text',
        text: 'Deep Leaf',
      });

      const deepPayload = {
        createSurface: {
          components: deepComponents,
        },
      };

      const deepSurface: A2uiSurface = {
        surfaceId: 'surf-deep',
        publishSeq: 1,
        payload: deepPayload,
        action: null,
      };

      expect(() => {
        renderCard(deepSurface);
      }).not.toThrow();

      const errorAlert = screen.getByRole('alert');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent(/exceeds maximum depth/i);
    });

    it('hits component expansion visit budget on exponential DAG without hanging', () => {
      // Doubling DAG: 14 nodes, depth 14, but 2^13 - 1 > 4096 visits
      const dagComponents: Array<Record<string, unknown>> = [];
      for (let i = 0; i < 13; i++) {
        dagComponents.push({
          id: `dag-node-${i}`,
          component: 'Column',
          children: [`dag-node-${i + 1}`, `dag-node-${i + 1}`],
        });
      }
      dagComponents.push({
        id: 'dag-node-13',
        component: 'Text',
        text: 'DAG Leaf',
      });

      const dagPayload = {
        createSurface: {
          components: [
            {
              id: 'root',
              component: 'Column',
              children: ['dag-node-0'],
            },
            ...dagComponents,
          ],
        },
      };

      const dagSurface: A2uiSurface = {
        surfaceId: 'surf-exp-dag',
        publishSeq: 1,
        payload: dagPayload,
        action: null,
      };

      const startTime = Date.now();
      expect(() => {
        renderCard(dagSurface);
      }).not.toThrow();
      const elapsed = Date.now() - startTime;
      expect(elapsed).toBeLessThan(1000); // Bounded: does not hang!

      const errorAlert = screen.getByRole('alert');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent(/maximum expansion/i);
    });

    it('renders fallback error card when A2UI form crashes', () => {
      const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
      const spy = vi.spyOn(A2UIFormModule, 'A2UIForm').mockImplementation(() => {
        throw new Error('Form render boom');
      });

      try {
        renderCard();
        const errorAlert = screen.getByTestId('a2ui-surface-error');
        expect(errorAlert).toBeInTheDocument();
        expect(errorAlert).toHaveTextContent('Unable to render surface: Form render boom');
      } finally {
        spy.mockRestore();
        consoleError.mockRestore();
      }
    });

    it('catches child render error in SurfaceErrorBoundary within A2uiSurfaceCard preserving sibling card', () => {
      const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
      const originalA2UIForm = A2UIFormModule.A2UIForm;
      const spy = vi.spyOn(A2UIFormModule, 'A2UIForm').mockImplementation((props) => {
        if ((props.payload as any)?.createSurface?.surfaceId === 'surf-crash') {
          throw new Error('Crash in surf-crash');
        }
        return originalA2UIForm(props);
      });

      const sNormal: A2uiSurface = {
        surfaceId: 'surf-normal',
        publishSeq: 1,
        payload: testPayload,
        action: null,
      };
      const sCrash: A2uiSurface = {
        surfaceId: 'surf-crash',
        publishSeq: 1,
        payload: {
          createSurface: {
            surfaceId: 'surf-crash',
            components: [{ id: 'root', component: 'Text', text: 'Will crash' }],
          },
        },
        action: null,
      };

      try {
        render(
          <div>
            <A2uiSurfaceCard surface={sCrash} />
            <A2uiSurfaceCard surface={sNormal} />
          </div>
        );

        const errorAlert = screen.getByTestId('a2ui-surface-error');
        expect(errorAlert).toBeInTheDocument();
        expect(errorAlert).toHaveTextContent('Unable to render surface: Crash in surf-crash');
        expect(screen.getByText('Deploy Service')).toBeInTheDocument();
      } finally {
        spy.mockRestore();
        consoleError.mockRestore();
      }
    });

    it('scopes DOM ids and radio-group names per surface so identical component ids do not collide', () => {
      const sharedPayload = {
        createSurface: {
          dataModel: {
            env: 'staging',
            plan: 'pro',
          },
          components: [
            {
              id: 'root',
              component: 'Column',
              children: ['env-field', 'plan-picker'],
            },
            {
              id: 'env-field',
              component: 'TextField',
              label: 'Environment Name',
              value: { '@path': '/env' },
            },
            {
              id: 'plan-picker',
              component: 'ChoicePicker',
              label: 'Plan Tier',
              value: { '@path': '/plan' },
              variant: 'mutuallyExclusive',
              options: [
                { label: 'Basic', value: 'basic' },
                { label: 'Pro', value: 'pro' },
              ],
            },
          ],
        },
      };

      const s1: A2uiSurface = {
        surfaceId: 'surf-dup-1',
        publishSeq: 1,
        payload: sharedPayload,
        action: null,
      };
      const s2: A2uiSurface = {
        surfaceId: 'surf-dup-2',
        publishSeq: 1,
        payload: sharedPayload,
        action: null,
      };

      const surfacesMap = new Map([
        ['surf-dup-1:1', s1],
        ['surf-dup-2:1', s2],
      ]);

      const surfacesContext = {
        agentName: 'agent-1',
        sessionId: 'session-1',
        surfaces: surfacesMap,
        getSurfacesForToolCall: () => [],
        unattachedSurfaces: [s1, s2],
        updateSurfaceResolution: vi.fn(),
      };

      render(
        <A2uiSurfacesContext.Provider value={surfacesContext}>
          <div data-testid="card-1">
            <A2uiSurfaceCard surface={s1} />
          </div>
          <div data-testid="card-2">
            <A2uiSurfaceCard surface={s2} />
          </div>
        </A2uiSurfacesContext.Provider>
      );

      const card1 = screen.getByTestId('card-1');
      const card2 = screen.getByTestId('card-2');

      // Check text field input IDs
      const input1 = card1.querySelector('input[type="text"]:not([aria-label*="Filter"])') as HTMLInputElement;
      const input2 = card2.querySelector('input[type="text"]:not([aria-label*="Filter"])') as HTMLInputElement;
      expect(input1).toBeInTheDocument();
      expect(input2).toBeInTheDocument();
      expect(input1.id).toBeTruthy();
      expect(input2.id).toBeTruthy();
      expect(input1.id).not.toEqual(input2.id);

      // Check corresponding labels match their own input id
      const label1 = card1.querySelector('label[for="' + input1.id + '"]');
      const label2 = card2.querySelector('label[for="' + input2.id + '"]');
      expect(label1).toBeInTheDocument();
      expect(label2).toBeInTheDocument();

      // Check radio buttons and group names
      const radios1 = card1.querySelectorAll('input[type="radio"]');
      const radios2 = card2.querySelectorAll('input[type="radio"]');
      expect(radios1.length).toBe(2);
      expect(radios2.length).toBe(2);

      const radio1Group = (radios1[0] as HTMLInputElement).name;
      const radio2Group = (radios2[0] as HTMLInputElement).name;
      expect(radio1Group).toBeTruthy();
      expect(radio2Group).toBeTruthy();
      expect(radio1Group).not.toEqual(radio2Group);

      // Radio option IDs are also distinct
      const radio1OptionId = (radios1[0] as HTMLInputElement).id;
      const radio2OptionId = (radios2[0] as HTMLInputElement).id;
      expect(radio1OptionId).not.toEqual(radio2OptionId);
    });
  });

  describe('Integration with ToolCallCard', () => {
    it('renders attached non-modal surface inside ToolCallCard whether expanded or collapsed', () => {
      const surface: A2uiSurface = {
        surfaceId: 'surf-tool-1',
        publishSeq: 10,
        toolCallId: 'call-abc',
        payload: testPayload,
        action: null,
      };

      const surfacesContext = {
        agentName: 'agent-1',
        sessionId: 'session-1',
        surfaces: new Map([['surf-tool-1:10', surface]]),
        getSurfacesForToolCall: (id: string) => (id === 'call-abc' ? [surface] : []),
        unattachedSurfaces: [],
        updateSurfaceResolution: vi.fn(),
      };

      const usageContext = {
        usage: null,
        toolSummaries: new Map(),
        toolUpdates: new Map(),
      };

      render(
        <UsageContext.Provider value={usageContext}>
          <A2uiSurfacesContext.Provider value={surfacesContext}>
            <ToolCallCard
              toolCallId="call-abc"
              toolName="deployer"
              args={{ target: 'prod' }}
              status={{ type: 'complete' }}
            />
          </A2uiSurfacesContext.Provider>
        </UsageContext.Provider>
      );

      // Visible when collapsed
      expect(screen.getByTestId('a2ui-surface-surf-tool-1-10')).toBeInTheDocument();
      expect(screen.getByText('Deploy Service')).toBeInTheDocument();

      // Click header to toggle expand
      const header = screen.getByRole('button', { name: /deployer/i });
      fireEvent.click(header);

      // Still visible when expanded
      expect(screen.getByTestId('a2ui-surface-surf-tool-1-10')).toBeInTheDocument();
      expect(screen.getByText('Deploy Service')).toBeInTheDocument();
    });

    it('multiple surfaces can be attached to different tool calls independently', () => {
      const surface1: A2uiSurface = {
        surfaceId: 'surf-1',
        publishSeq: 1,
        toolCallId: 'call-1',
        payload: testPayload,
        action: null,
      };
      const surface2: A2uiSurface = {
        surfaceId: 'surf-2',
        publishSeq: 2,
        toolCallId: 'call-2',
        payload: {
          version: '1.0',
          createSurface: {
            surfaceId: 'surf-2',
            components: [
              { id: 'root', component: 'Card', child: 'txt' },
              { id: 'txt', component: 'Text', text: 'Second Surface' },
            ],
          },
        },
        action: null,
      };

      const surfacesMap = new Map([
        ['surf-1:1', surface1],
        ['surf-2:2', surface2],
      ]);

      const surfacesContext = {
        agentName: 'agent-1',
        sessionId: 'session-1',
        surfaces: surfacesMap,
        getSurfacesForToolCall: (id: string) => {
          if (id === 'call-1') return [surface1];
          if (id === 'call-2') return [surface2];
          return [];
        },
        unattachedSurfaces: [],
        updateSurfaceResolution: vi.fn(),
      };

      const usageContext = {
        usage: null,
        toolSummaries: new Map(),
        toolUpdates: new Map(),
      };

      render(
        <UsageContext.Provider value={usageContext}>
          <A2uiSurfacesContext.Provider value={surfacesContext}>
            <div>
              <ToolCallCard toolCallId="call-1" toolName="tool1" status={{ type: 'complete' }} />
              <ToolCallCard toolCallId="call-2" toolName="tool2" status={{ type: 'complete' }} />
            </div>
          </A2uiSurfacesContext.Provider>
        </UsageContext.Provider>
      );

      expect(screen.getByTestId('a2ui-surface-surf-1-1')).toBeInTheDocument();
      expect(screen.getByTestId('a2ui-surface-surf-2-2')).toBeInTheDocument();
      expect(screen.getByText('Deploy Service')).toBeInTheDocument();
      expect(screen.getByText('Second Surface')).toBeInTheDocument();
    });
  });

  describe('Non-blocking behavior', () => {
    it('keeps conversation and composer non-blocked with multiple outstanding surfaces', () => {
      const surface1: A2uiSurface = {
        surfaceId: 'surf-nb-1',
        publishSeq: 1,
        toolCallId: 'call-1',
        payload: testPayload,
        action: null,
      };
      const surface2: A2uiSurface = {
        surfaceId: 'surf-nb-2',
        publishSeq: 2,
        toolCallId: 'call-2',
        payload: testPayload,
        action: null,
      };

      const surfacesMap = new Map([
        ['surf-nb-1:1', surface1],
        ['surf-nb-2:2', surface2],
      ]);

      const surfacesContext = {
        agentName: 'agent-1',
        sessionId: 'session-1',
        surfaces: surfacesMap,
        getSurfacesForToolCall: (id: string) => {
          if (id === 'call-1') return [surface1];
          if (id === 'call-2') return [surface2];
          return [];
        },
        unattachedSurfaces: [],
        updateSurfaceResolution: vi.fn(),
      };

      render(
        <A2uiSurfacesContext.Provider value={surfacesContext}>
          <div data-testid="thread-container">
            <A2uiSurfaceCard surface={surface1} />
            <A2uiSurfaceCard surface={surface2} />
            <div data-testid="composer-container">
              <input placeholder="Type a message..." aria-label="Chat input" />
              <button type="button">Send</button>
            </div>
          </div>
        </A2uiSurfacesContext.Provider>
      );

      // Both surfaces are outstanding and interactive
      expect(screen.getByTestId('a2ui-surface-surf-nb-1-1')).toBeInTheDocument();
      expect(screen.getByTestId('a2ui-surface-surf-nb-2-2')).toBeInTheDocument();

      // Composer remains completely functional and unblocked
      const input = screen.getByLabelText('Chat input') as HTMLInputElement;
      expect(input).not.toBeDisabled();
      fireEvent.change(input, { target: { value: 'Continuing conversation while surfaces are pending' } });
      expect(input.value).toBe('Continuing conversation while surfaces are pending');

      const sendBtn = screen.getByRole('button', { name: 'Send' });
      expect(sendBtn).not.toBeDisabled();
    });
  });

  describe('SurfaceErrorBoundary', () => {
    it('catches error during render and displays fallback alert while siblings render', () => {
      const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
      const ThrowingChild = () => {
        throw new Error('Component crashed unexpectedly');
      };

      render(
        <div>
          <div data-testid="sibling-container">Sibling still intact</div>
          <SurfaceErrorBoundary>
            <ThrowingChild />
          </SurfaceErrorBoundary>
        </div>
      );

      const errorAlert = screen.getByTestId('a2ui-surface-error');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent('Unable to render surface: Component crashed unexpectedly');
      expect(screen.getByTestId('sibling-container')).toHaveTextContent('Sibling still intact');

      consoleError.mockRestore();
    });

    it('renders fallback alert with default message when error has no message', () => {
      const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
      const ThrowingChild = () => {
        throw new Error('');
      };

      render(
        <SurfaceErrorBoundary>
          <ThrowingChild />
        </SurfaceErrorBoundary>
      );

      const errorAlert = screen.getByTestId('a2ui-surface-error');
      expect(errorAlert).toBeInTheDocument();
      expect(errorAlert).toHaveTextContent('Unable to render surface: Component render error');

      consoleError.mockRestore();
    });
  });
});
