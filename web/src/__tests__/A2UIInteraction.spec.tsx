import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { BatchInterruptUI } from '../App';
import { submitInteractionResponse, InteractionResponseError } from '../api';
import type { PendingInteraction } from '../harnxCustomEvents';
import { PendingContext } from '../PendingContext';
import * as agUi from '@assistant-ui/react-ag-ui';

vi.mock('@assistant-ui/react-ag-ui', async (importOriginal) => {
  const actual = await importOriginal<typeof agUi>();
  return { ...actual, useAgUiInterrupts: vi.fn(), useAgUiSubmitInterruptResponses: vi.fn() };
});

vi.mock('../api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api')>();
  return {
    ...actual,
    submitInteractionResponse: vi.fn(),
  };
});

const defaultPendingContext = {
  statusText: null,
  setStatusText: vi.fn(),
  errorText: null,
  setErrorText: vi.fn(),
  statusMessage: null,
  setStatusMessage: vi.fn(),
  hydratedApprovals: [],
  addHydratedApproval: vi.fn(),
  clearHydratedApprovals: vi.fn(),
  removeHydratedApproval: vi.fn(),
  pendingInteraction: null,
  setPendingInteraction: vi.fn(),
  clearPendingInteraction: vi.fn(),
};

describe('A2UI Interaction web renderer', () => {
  beforeEach(() => {
    vi.mocked(submitInteractionResponse).mockReset();
    vi.mocked(agUi.useAgUiInterrupts).mockReturnValue([]);
  });

  const fullSurfacePayload = {
    version: '1.0',
    createSurface: {
      dataModel: {
        choice: ['staging'],
        notes: 'Initial release note',
        counter: '42',
        secret: 'initial-pass',
        tags: ['tag1'],
      },
      components: [
        { id: 'root', component: 'Card', child: 'main-col' },
        {
          id: 'main-col',
          component: 'Column',
          justify: 'spaceBetween',
          align: 'stretch',
          children: [
            'header',
            'caption-text',
            'env-picker',
            'tags-picker',
            'release-name',
            'notes-area',
            'count-field',
            'secret-field',
            'btn-default',
            'btn-primary-label',
            'btn-primary',
            'btn-borderless-label',
            'btn-borderless',
          ],
        },
        { id: 'header', component: 'Text', text: 'Deploy Application' },
        { id: 'caption-text', component: 'Text', text: 'Select target and settings', variant: 'caption' },
        {
          id: 'env-picker',
          component: 'ChoicePicker',
          label: 'Environment',
          value: { '@path': '/choice' },
          variant: 'mutuallyExclusive',
          filterable: true,
          options: [
            { label: 'Staging Server', value: 'staging' },
            { label: 'Production Cluster', value: 'prod' },
            { label: 'Dev Box', value: 'dev' },
          ],
        },
        {
          id: 'tags-picker',
          component: 'ChoicePicker',
          label: 'Tags',
          value: { '@path': '/tags' },
          variant: 'multipleSelection',
          displayStyle: 'chips',
          options: [
            { label: 'Tag One', value: 'tag1' },
            { label: 'Tag Two', value: 'tag2' },
          ],
        },
        {
          id: 'release-name',
          component: 'TextField',
          label: 'Release Name',
          value: { '@path': '/name' },
          placeholder: 'e.g. v1.0.0',
          variant: 'shortText',
        },
        {
          id: 'notes-area',
          component: 'TextField',
          label: 'Release Notes',
          value: { '@path': '/notes' },
          variant: 'longText',
        },
        {
          id: 'count-field',
          component: 'TextField',
          label: 'Instance Count',
          value: { '@path': '/counter' },
          variant: 'number',
        },
        {
          id: 'secret-field',
          component: 'TextField',
          label: 'Auth Token',
          value: { '@path': '/secret' },
          variant: 'obscured',
        },
        {
          id: 'btn-default',
          component: 'Button',
          child: 'Save Draft',
          variant: 'default',
          action: {
            event: {
              name: 'save_draft',
            },
          },
        },
        { id: 'btn-primary-label', component: 'Text', text: 'Deploy Now' },
        {
          id: 'btn-primary',
          component: 'Button',
          child: 'btn-primary-label',
          variant: 'primary',
          action: {
            event: {
              name: 'deploy',
              context: {
                target: { '@path': '/choice' },
                version: { '@path': '/name' },
                staticMeta: 'cluster-eu',
              },
            },
          },
        },
        { id: 'btn-borderless-label', component: 'Text', text: 'Quick Test' },
        {
          id: 'btn-borderless',
          component: 'Button',
          child: 'btn-borderless-label',
          variant: 'borderless',
          action: {
            event: {
              name: 'quick_test',
            },
          },
        },
      ],
    },
  };

  const sampleA2uiInteraction: PendingInteraction = {
    interactionId: 'int-a2ui-1',
    requestSeq: 7,
    toolCallId: 'tool-deploy',
    kind: 'a2ui',
    message: 'Deploy Application',
    pendingCount: 0,
    payload: fullSurfacePayload,
  };

  it('renders all components in the supported subset (Card, Column, Text, Button, ChoicePicker, TextField)', () => {
    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: sampleA2uiInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    // Card & Column
    expect(screen.getByTestId('a2ui-card-root')).toBeInTheDocument();
    expect(screen.getByTestId('a2ui-column-main-col')).toBeInTheDocument();

    // Text (body and caption)
    expect(screen.getByText('Deploy Application')).toHaveClass('a2ui-text-body');
    expect(screen.getByText('Select target and settings')).toHaveClass('a2ui-text-caption');

    // TextField variants (shortText, longText, number, obscured)
    expect(screen.getByLabelText('Release Name')).toHaveAttribute('type', 'text');
    expect(screen.getByPlaceholderText('e.g. v1.0.0')).toBeInTheDocument();
    expect(screen.getByLabelText('Release Notes').tagName.toLowerCase()).toBe('textarea');
    expect(screen.getByLabelText('Instance Count')).toHaveAttribute('type', 'number');
    expect(screen.getByLabelText('Auth Token')).toHaveAttribute('type', 'password');

    // ChoicePickers
    expect(screen.getByText('Environment')).toBeInTheDocument();
    expect(screen.getByLabelText('Staging Server')).toHaveAttribute('type', 'radio');
    expect(screen.getByLabelText('Production Cluster')).toHaveAttribute('type', 'radio');
    expect(screen.getByLabelText('Dev Box')).toHaveAttribute('type', 'radio');
    expect(screen.getByLabelText('Tag One')).toHaveAttribute('type', 'checkbox');
    expect(screen.getByLabelText('Tag Two')).toHaveAttribute('type', 'checkbox');

    // Filterable input on ChoicePicker
    expect(screen.getByPlaceholderText('Filter options...')).toBeInTheDocument();

    // Buttons and variants
    const saveBtn = screen.getByTestId('a2ui-btn-save_draft');
    expect(saveBtn).toHaveClass('a2ui-btn-default');
    expect(saveBtn).toHaveTextContent('Save Draft');

    const deployBtn = screen.getByTestId('a2ui-btn-deploy');
    expect(deployBtn).toHaveClass('a2ui-btn-primary');
    expect(deployBtn).toHaveTextContent('Deploy Now');

    const quickTestBtn = screen.getByTestId('a2ui-btn-quick_test');
    expect(quickTestBtn).toHaveClass('a2ui-btn-borderless');
    expect(quickTestBtn).toHaveTextContent('Quick Test');

    // Deny button is always present
    expect(screen.getByRole('button', { name: 'Deny' })).toBeInTheDocument();
  });

  it('populates initial form values from dataModel', () => {
    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: sampleA2uiInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    expect(screen.getByLabelText('Release Notes')).toHaveValue('Initial release note');
    expect(screen.getByLabelText('Instance Count')).toHaveValue(42);
    expect(screen.getByLabelText('Auth Token')).toHaveValue('initial-pass');
    expect(screen.getByLabelText('Staging Server')).toBeChecked();
    expect(screen.getByLabelText('Production Cluster')).not.toBeChecked();
    expect(screen.getByLabelText('Tag One')).toBeChecked();
    expect(screen.getByLabelText('Tag Two')).not.toBeChecked();
  });

  it('filters ChoicePicker options when typing into filter input', () => {
    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: sampleA2uiInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    const filterInput = screen.getByPlaceholderText('Filter options...');
    expect(screen.getByLabelText('Staging Server')).toBeInTheDocument();
    expect(screen.getByLabelText('Production Cluster')).toBeInTheDocument();
    expect(screen.getByLabelText('Dev Box')).toBeInTheDocument();

    fireEvent.change(filterInput, { target: { value: 'Cluster' } });
    expect(screen.queryByLabelText('Staging Server')).not.toBeInTheDocument();
    expect(screen.getByLabelText('Production Cluster')).toBeInTheDocument();
    expect(screen.queryByLabelText('Dev Box')).not.toBeInTheDocument();
  });

  it('submits action with input values and builds canonical data according to contract', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleA2uiInteraction,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    // Edit fields
    fireEvent.change(screen.getByLabelText('Release Name'), { target: { value: 'v2.1.0' } });
    fireEvent.click(screen.getByLabelText('Production Cluster'));
    fireEvent.click(screen.getByLabelText('Tag Two'));

    // Click Deploy Now button
    fireEvent.click(screen.getByTestId('a2ui-btn-deploy'));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith('agent-x', 'session-y', {
        interactionId: 'int-a2ui-1',
        requestSeq: 7,
        response: {
          action: 'deploy',
          data: {
            choice: ['prod'],
            name: 'v2.1.0',
            notes: 'Initial release note',
            counter: '42',
            secret: 'initial-pass',
            tags: ['tag1', 'tag2'],
            target: ['prod'],
            version: 'v2.1.0',
            staticMeta: 'cluster-eu',
          },
          note: undefined,
        },
      });
    });

    const updater = setPendingInteraction.mock.calls[0][0];
    expect(typeof updater).toBe('function');
    expect(updater(sampleA2uiInteraction)).toBeNull();
  });

  it('submits action with optional note when entered', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });

    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: sampleA2uiInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    fireEvent.change(screen.getByPlaceholderText('Reason for approval or denial...'), {
      target: { value: 'Approved for scheduled deployment' },
    });

    fireEvent.click(screen.getByTestId('a2ui-btn-deploy'));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith(
        'agent-x',
        'session-y',
        expect.objectContaining({
          response: expect.objectContaining({
            action: 'deploy',
            note: 'Approved for scheduled deployment',
          }),
        })
      );
    });
  });

  it('submits deny with empty data even when form inputs are filled', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });

    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: sampleA2uiInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    fireEvent.change(screen.getByLabelText('Release Name'), { target: { value: 'custom-name' } });
    fireEvent.change(screen.getByPlaceholderText('Reason for approval or denial...'), {
      target: { value: 'Deployment cancelled by operator' },
    });

    fireEvent.click(screen.getByRole('button', { name: 'Deny' }));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith('agent-x', 'session-y', {
        interactionId: 'int-a2ui-1',
        requestSeq: 7,
        response: {
          action: 'deny',
          data: {},
          note: 'Deployment cancelled by operator',
        },
      });
    });
  });

  it('displays server rejection error and allows correcting input and resubmitting', async () => {
    const rejectionError = new InteractionResponseError(
      'interaction response rejected',
      -32004,
      'invalid_response',
      { outcome: 'invalid_response' }
    );
    vi.mocked(submitInteractionResponse).mockRejectedValueOnce(rejectionError);

    const setErrorText = vi.fn();
    const setPendingInteraction = vi.fn();

    render(
      <PendingContext.Provider
        value={{
          ...defaultPendingContext,
          pendingInteraction: sampleA2uiInteraction,
          setErrorText,
          setPendingInteraction,
        }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    // Initial submit rejects
    fireEvent.click(screen.getByTestId('a2ui-btn-deploy'));

    await waitFor(() => {
      expect(screen.getByRole('alert')).toHaveTextContent(
        'Interaction response rejected: invalid_response'
      );
    });
    expect(setErrorText).toHaveBeenCalledWith('Interaction response rejected: invalid_response');
    expect(setPendingInteraction).not.toHaveBeenCalled();

    // Inputs remain interactive; user corrects input and resubmits
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });

    fireEvent.change(screen.getByLabelText('Release Name'), { target: { value: 'corrected-v2' } });
    fireEvent.click(screen.getByTestId('a2ui-btn-deploy'));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledTimes(2);
      expect(submitInteractionResponse).toHaveBeenLastCalledWith(
        'agent-x',
        'session-y',
        expect.objectContaining({
          response: expect.objectContaining({
            action: 'deploy',
            data: expect.objectContaining({
              name: 'corrected-v2',
            }),
          }),
        })
      );
    });
  });

  it('renders fallback when a2ui interaction is missing payload', async () => {
    vi.mocked(submitInteractionResponse).mockResolvedValueOnce({ applied: true, outcome: 'applied' });

    const missingPayloadInteraction: PendingInteraction = {
      interactionId: 'int-no-payload',
      requestSeq: 2,
      toolCallId: 'tool-orphan',
      kind: 'a2ui',
      message: 'A2UI Form without payload',
      pendingCount: 0,
      payload: undefined,
    };

    render(
      <PendingContext.Provider
        value={{ ...defaultPendingContext, pendingInteraction: missingPayloadInteraction }}
      >
        <BatchInterruptUI agentName="agent-x" sessionId="session-y" />
      </PendingContext.Provider>
    );

    expect(screen.getByText('Unable to render form')).toBeInTheDocument();
    expect(screen.getByText('A2UI Form without payload')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Deny' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Deny' }));

    await waitFor(() => {
      expect(submitInteractionResponse).toHaveBeenCalledWith('agent-x', 'session-y', {
        interactionId: 'int-no-payload',
        requestSeq: 2,
        response: {
          action: 'deny',
          data: {},
          note: undefined,
        },
      });
    });
  });
});
