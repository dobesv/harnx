import { render, screen, fireEvent } from '@testing-library/react';
import { describe, it, expect, vi } from 'vitest';
import { ToolCallCard } from './ToolCallCard';
import { UsageContext } from './UsageContext';

vi.mock('@assistant-ui/react-markdown', () => ({
  MarkdownTextPrimitive: ({ text, children }: any) => <div>{text}{children}</div>
}));

vi.mock('react-markdown', () => ({
  default: ({ children, components }: any) => {
    if (typeof children === 'string' && /\[(.*?)\]\((.*?)\)/.test(children)) {
      const match = children.match(/\[(.*?)\]\((.*?)\)/);
      if (match) {
        const [full, linkText, href] = match;
        const [before, after] = children.split(full);
        const LinkComp = components?.a || 'a';
        return (
          <div>
            {before}
            <LinkComp href={href}>{linkText}</LinkComp>
            {after}
          </div>
        );
      }
    }
    return <div>{children}</div>;
  }
}));

vi.mock('react-json-view-lite', () => ({
  JsonView: ({ data }: any) => <div data-testid="json-view">{JSON.stringify(data)}</div>,
  darkStyles: {},
  defaultStyles: {}
}));

describe('ToolCallCard', () => {
  function renderWithContext(props: any, summaries = new Map()) {
    return render(
      <UsageContext.Provider value={{ usage: null, toolSummaries: summaries }}>
        <ToolCallCard {...props} />
      </UsageContext.Provider>
    );
  }

  it('renders correctly with basic props', () => {
    const props = {
      toolName: 'my_tool',
      args: { a: 1 },
      toolCallId: 'call_1',
      status: { type: 'complete' }
    } as any;
    
    renderWithContext(props);
    expect(screen.getByText('my_tool')).toBeInTheDocument();
  });

  it('shows summary from context', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      status: { type: 'complete' }
    } as any;
    const summaries = new Map([['call_1', 'my summary markdown']]);
    
    renderWithContext(props, summaries);
    expect(screen.getByText('my summary markdown')).toBeInTheDocument();
  });

  it('shows summary from result markdown', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      result: { markdown: 'restored summary' },
      status: { type: 'complete' }
    } as any;
    
    renderWithContext(props);
    expect(screen.getByText('restored summary')).toBeInTheDocument();
  });

  it('shows command preview from args when no summary is present', () => {
    const props = {
      toolName: 'bash_exec',
      toolCallId: 'call_1',
      args: { command: 'echo "hello"' },
      status: { type: 'complete' }
    } as any;
    
    renderWithContext(props);
    expect(screen.getByText('$ echo "hello"')).toBeInTheDocument();
  });

  it('shows generic arg preview from single arg when no summary is present', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      args: { single_arg: 'just a string' },
      status: { type: 'complete' }
    } as any;
    
    renderWithContext(props);
    expect(screen.getByText('just a string')).toBeInTheDocument();
  });

  it('shows generic json preview from args when no summary is present', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      args: { a: 1, b: 2 },
      status: { type: 'complete' }
    } as any;
    
    renderWithContext(props);
    expect(screen.getByText('{"a":1,"b":2}')).toBeInTheDocument();
  });

  it('summary markdown takes precedence over args preview', () => {
    const props = {
      toolName: 'bash_exec',
      toolCallId: 'call_1',
      args: { command: 'echo "hello"' },
      status: { type: 'complete' }
    } as any;
    const summaries = new Map([['call_1', 'my summary markdown']]);
    
    renderWithContext(props, summaries);
    expect(screen.getByText('my summary markdown')).toBeInTheDocument();
    expect(screen.queryByText('$ echo "hello"')).toBeNull();
  });

  it('collapses and expands', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      status: { type: 'complete' }
    } as any;
    
    const { container } = renderWithContext(props);
    
    // By default complete is collapsed
    expect(screen.queryByText('Tree View')).toBeNull();

    // Click header to expand
    fireEvent.click(container.querySelector('.aui-tool-call-header')!);
    expect(screen.getByText('View Source')).toBeInTheDocument();
  });

  it('applies collapsed line-clamp class when collapsed and expanded class when expanded', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      status: { type: 'complete' }
    } as any;
    const summaries = new Map([['call_1', 'line 1\nline 2\nline 3\nline 4\nline 5']]);

    const { container } = renderWithContext(props, summaries);
    const summary = container.querySelector('.aui-tool-summary');
    expect(summary).toHaveClass('aui-tool-summary-collapsed');
    expect(summary).not.toHaveClass('aui-tool-summary-expanded');

    // Click to expand
    fireEvent.click(container.querySelector('.aui-tool-call-header')!);
    expect(summary).toHaveClass('aui-tool-summary-expanded');
    expect(summary).not.toHaveClass('aui-tool-summary-collapsed');
  });

  it('renders markdown result formatted in expanded view with View Source toggle', () => {
    const props = {
      toolName: 'bash_exec',
      toolCallId: 'call_1',
      args: { command: 'ls' },
      result: '## Files\n- file1.txt\n- file2.txt',
      status: { type: 'complete' }
    } as any;

    const { container } = renderWithContext(props);
    // Expand the card
    fireEvent.click(container.querySelector('.aui-tool-call-header')!);

    expect(screen.getByText('Result:')).toBeInTheDocument();
    expect(screen.getByText(/file1\.txt/)).toBeInTheDocument();
    expect(container.querySelector('.aui-tool-result-markdown')).toBeInTheDocument();

    // Toggle View Source
    fireEvent.click(screen.getByText('View Source'));
    expect(screen.getByText('Tree View')).toBeInTheDocument();
    expect(screen.getByText('Raw Result:')).toBeInTheDocument();

    // Toggle back to Tree View / formatted view
    fireEvent.click(screen.getByText('Tree View'));
    expect(screen.getByText('View Source')).toBeInTheDocument();
    expect(container.querySelector('.aui-tool-result-markdown')).toBeInTheDocument();
  });

  it('renders structured object results using JsonView in expanded view', () => {
    const props = {
      toolName: 'json_tool',
      toolCallId: 'call_1',
      args: { query: 'test' },
      result: { count: 42, items: ['alpha', 'beta'] },
      status: { type: 'complete' }
    } as any;

    const { container } = renderWithContext(props);
    fireEvent.click(container.querySelector('.aui-tool-call-header')!);

    expect(screen.getByText('Result:')).toBeInTheDocument();
    expect(screen.getByText('{"count":42,"items":["alpha","beta"]}')).toBeInTheDocument();
  });

  it('clicking a markdown link inside the tool-call header summary does NOT toggle card expansion (B3)', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      status: { type: 'complete' }
    } as any;
    const summaries = new Map([['call_1', 'Check [Docs](https://example.com)']]);

    const { container } = renderWithContext(props, summaries);
    const header = container.querySelector('.aui-tool-call-header')!;
    expect(header).toHaveAttribute('aria-expanded', 'false');

    const link = screen.getByRole('link', { name: 'Docs' });
    expect(link.closest('.aui-tool-call-header')).not.toBeNull();
    expect(link).toHaveAttribute('target', '_blank');
    expect(link).toHaveAttribute('rel', 'noopener noreferrer');

    // Click the link inside the header
    fireEvent.click(link);

    // Header should still be collapsed (link click did not toggle accordion)
    expect(header).toHaveAttribute('aria-expanded', 'false');
  });

  it('renders markdown links with target="_blank" in expanded result body', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      args: { a: 1 },
      result: 'Visit [Documentation](https://example.com) for details',
      status: { type: 'complete' }
    } as any;

    const { container } = renderWithContext(props);
    const header = container.querySelector('.aui-tool-call-header')!;

    // Expand the card
    fireEvent.click(header);
    expect(header).toHaveAttribute('aria-expanded', 'true');

    const link = screen.getByRole('link', { name: 'Documentation' });
    expect(link).toHaveAttribute('target', '_blank');
    expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  });

  it('activating a markdown link via Enter or Space does not toggle header expansion (E1)', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_1',
      status: { type: 'complete' }
    } as any;
    const summaries = new Map([['call_1', 'Check [Link](https://example.com)']]);

    const { container } = renderWithContext(props, summaries);
    const header = container.querySelector('.aui-tool-call-header')!;
    expect(header).toHaveAttribute('aria-expanded', 'false');

    const link = screen.getByRole('link', { name: 'Link' });
    
    // Pressing Enter on the link should NOT toggle the header
    fireEvent.keyDown(link, { key: 'Enter' });
    expect(header).toHaveAttribute('aria-expanded', 'false');

    // Pressing Space on the link should NOT toggle the header
    fireEvent.keyDown(link, { key: ' ' });
    expect(header).toHaveAttribute('aria-expanded', 'false');

    // Pressing Enter directly on the header DOES toggle it
    fireEvent.keyDown(header, { key: 'Enter' });
    expect(header).toHaveAttribute('aria-expanded', 'true');
  });

  describe('sub-agent call rendering and default expansion', () => {
    it('sub-agent call starts expanded by default and renders prompt and response', () => {
      const props = {
        toolName: 'researcher_session_prompt',
        toolCallId: 'call_sub_1',
        args: { message: 'Investigate the backend architecture', session_id: 'sub_sess_99' },
        result: JSON.stringify({ response: 'Here are the architectural findings.' }),
        status: { type: 'complete' }
      } as any;
      const summaries = new Map([
        ['call_sub_1', '@ researcher [sub_sess_99]\nInvestigate the backend architecture']
      ]);

      const { container } = renderWithContext(props, summaries);

      // Verify sub-agent card is expanded without user interaction
      const header = container.querySelector('.aui-tool-call-header');
      expect(header).toHaveAttribute('aria-expanded', 'true');
      expect(screen.getByText('View Source')).toBeInTheDocument();

      // Verify Prompt and Response are both rendered as markdown
      expect(screen.getByText('Prompt:')).toBeInTheDocument();
      expect(screen.getByText('Investigate the backend architecture')).toBeInTheDocument();
      expect(container.querySelector('.aui-tool-prompt-markdown')).toBeInTheDocument();

      expect(screen.getByText('Response:')).toBeInTheDocument();
      expect(screen.getByText('Here are the architectural findings.')).toBeInTheDocument();
      expect(container.querySelector('.aui-tool-result-markdown')).toBeInTheDocument();
    });

    it('running sub-agent call starts expanded and shows spinner and prompt', () => {
      const props = {
        toolName: 'researcher_session_prompt',
        toolCallId: 'call_sub_running',
        args: { message: 'Analyze the database schema', session_id: 'sub_sess_42' },
        status: { type: 'running' }
      } as any;

      const { container } = renderWithContext(props);

      const header = container.querySelector('.aui-tool-call-header');
      const icon = container.querySelector('.aui-tool-call-icon');
      const card = container.querySelector('.aui-tool-call') as HTMLElement;

      expect(header).toHaveAttribute('aria-expanded', 'true');
      expect(icon?.textContent).toBe('⏳');
      expect(card.style.borderLeft).toBe('4px solid var(--status-running)');

      expect(screen.getByText('Prompt:')).toBeInTheDocument();
      expect(screen.getByText('Analyze the database schema')).toBeInTheDocument();
    });

    it('sub-agent call identified by @ template in summary expands by default', () => {
      const props = {
        toolName: 'custom_dispatch',
        toolCallId: 'call_custom_sub',
        args: { message: 'Run child task' },
        result: { response: 'Child task finished.' },
        status: { type: 'complete' }
      } as any;
      const summaries = new Map([
        ['call_custom_sub', '@ worker [sess-child]\nRun child task']
      ]);

      const { container } = renderWithContext(props, summaries);

      const header = container.querySelector('.aui-tool-call-header');
      expect(header).toHaveAttribute('aria-expanded', 'true');
      expect(screen.getByText('Prompt:')).toBeInTheDocument();
      expect(screen.getByText('Response:')).toBeInTheDocument();
      expect(screen.getByText('Child task finished.')).toBeInTheDocument();
    });
  });

  describe('status presentation and initial expansion', () => {
    const cases = [
      {
        scenario: 'requires-action with reason tool-calls',
        status: { type: 'requires-action', reason: 'tool-calls' },
        isError: undefined,
        expectedIcon: '⏳',
        expectedExpanded: 'false',
        expectedBorder: '4px solid var(--status-running)',
      },
      {
        scenario: 'requires-action with reason interrupt',
        status: { type: 'requires-action', reason: 'interrupt' },
        isError: undefined,
        expectedIcon: '⚠️',
        expectedExpanded: 'true',
        expectedBorder: '4px solid var(--status-action)',
      },
      {
        scenario: 'running',
        status: { type: 'running' },
        isError: undefined,
        expectedIcon: '⏳',
        expectedExpanded: 'false',
        expectedBorder: '4px solid var(--status-running)',
      },
      {
        scenario: 'complete with no error',
        status: { type: 'complete' },
        isError: undefined,
        expectedIcon: '✅',
        expectedExpanded: 'false',
        expectedBorder: '4px solid var(--status-complete)',
      },
      {
        scenario: 'complete with isError true',
        status: { type: 'complete' },
        isError: true,
        expectedIcon: '❌',
        expectedExpanded: 'false',
        expectedBorder: '4px solid var(--status-complete)',
      },
      {
        scenario: 'incomplete status with error',
        status: { type: 'incomplete' },
        isError: true,
        expectedIcon: '❌',
        expectedExpanded: 'false',
        expectedBorder: '4px solid var(--status-error)',
      },
    ];

    it.each(cases)(
      'renders correct icon, expansion, and border for $scenario',
      ({ status, isError, expectedIcon, expectedExpanded, expectedBorder }) => {
        const props = {
          toolName: 'my_tool',
          toolCallId: 'call_1',
          status,
          isError,
        } as any;

        const { container } = renderWithContext(props);
        const icon = container.querySelector('.aui-tool-call-icon');
        const header = container.querySelector('.aui-tool-call-header');
        const card = container.querySelector('.aui-tool-call') as HTMLElement;

        expect(icon?.textContent).toBe(expectedIcon);
        expect(header).toHaveAttribute('aria-expanded', expectedExpanded);
        expect(card.style.borderLeft).toBe(expectedBorder);
      },
    );
  });
});
