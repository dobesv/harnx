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
});
