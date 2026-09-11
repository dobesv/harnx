import { render, screen, fireEvent } from '@testing-library/react';
import { describe, it, expect, vi } from 'vitest';
import { ToolCallCard } from './ToolCallCard';
import { UsageContext } from './UsageContext';

vi.mock('@assistant-ui/react-markdown', () => ({
  MarkdownTextPrimitive: ({ text, children }: any) => <div>{text}{children}</div>
}));

vi.mock('react-markdown', () => ({
  default: ({ children }: any) => <div>{children}</div>
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
      status: { type: 'running' }
    } as any;
    const summaries = new Map([['call_1', 'my summary markdown']]);
    
    renderWithContext(props, summaries);
    expect(screen.getByText('my summary markdown')).toBeInTheDocument();
  });

  it('shows summary from restored result.markdown', () => {
    const props = {
      toolName: 'my_tool',
      toolCallId: 'call_2',
      status: { type: 'complete' },
      result: { markdown: 'restored summary' }
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

  describe('status presentation and initial expansion', () => {
    it('renders pending spinner ⏳ and is collapsed for requires-action with reason tool-calls', () => {
      const props = {
        toolName: 'my_tool',
        toolCallId: 'call_1',
        status: { type: 'requires-action', reason: 'tool-calls' }
      } as any;

      const { container } = renderWithContext(props);
      const icon = container.querySelector('.aui-tool-call-icon');
      const header = container.querySelector('.aui-tool-call-header');
      const card = container.querySelector('.aui-tool-call') as HTMLElement;

      expect(icon?.textContent).toBe('⏳');
      expect(icon?.textContent).not.toBe('⚠️');
      expect(header).toHaveAttribute('aria-expanded', 'false');
      expect(card.style.borderLeft).toBe('4px solid var(--status-running)');
    });

    it('renders alert icon ⚠️ and is expanded for requires-action with reason interrupt', () => {
      const props = {
        toolName: 'my_tool',
        toolCallId: 'call_1',
        status: { type: 'requires-action', reason: 'interrupt' }
      } as any;

      const { container } = renderWithContext(props);
      const icon = container.querySelector('.aui-tool-call-icon');
      const header = container.querySelector('.aui-tool-call-header');
      const card = container.querySelector('.aui-tool-call') as HTMLElement;

      expect(icon?.textContent).toBe('⚠️');
      expect(header).toHaveAttribute('aria-expanded', 'true');
      expect(card.style.borderLeft).toBe('4px solid var(--status-action)');
    });

    it('renders pending spinner ⏳ and is collapsed for running', () => {
      const props = {
        toolName: 'my_tool',
        toolCallId: 'call_1',
        status: { type: 'running' }
      } as any;

      const { container } = renderWithContext(props);
      const icon = container.querySelector('.aui-tool-call-icon');
      const header = container.querySelector('.aui-tool-call-header');
      const card = container.querySelector('.aui-tool-call') as HTMLElement;

      expect(icon?.textContent).toBe('⏳');
      expect(header).toHaveAttribute('aria-expanded', 'false');
      expect(card.style.borderLeft).toBe('4px solid var(--status-running)');
    });

    it('renders checkmark ✅ and is collapsed for complete with no error', () => {
      const props = {
        toolName: 'my_tool',
        toolCallId: 'call_1',
        status: { type: 'complete' }
      } as any;

      const { container } = renderWithContext(props);
      const icon = container.querySelector('.aui-tool-call-icon');
      const header = container.querySelector('.aui-tool-call-header');
      const card = container.querySelector('.aui-tool-call') as HTMLElement;

      expect(icon?.textContent).toBe('✅');
      expect(header).toHaveAttribute('aria-expanded', 'false');
      expect(card.style.borderLeft).toBe('4px solid var(--status-complete)');
    });

    it('renders cross ❌ when isError is true', () => {
      const props = {
        toolName: 'my_tool',
        toolCallId: 'call_1',
        status: { type: 'complete' },
        isError: true
      } as any;

      const { container } = renderWithContext(props);
      const icon = container.querySelector('.aui-tool-call-icon');

      expect(icon?.textContent).toBe('❌');
    });

    it('renders error border for incomplete status with error', () => {
      const props = {
        toolName: 'my_tool',
        toolCallId: 'call_1',
        status: { type: 'incomplete' },
        isError: true
      } as any;

      const { container } = renderWithContext(props);
      const icon = container.querySelector('.aui-tool-call-icon');
      const card = container.querySelector('.aui-tool-call') as HTMLElement;

      expect(icon?.textContent).toBe('❌');
      expect(card.style.borderLeft).toBe('4px solid var(--status-error)');
    });
  });
});
