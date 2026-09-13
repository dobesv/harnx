import React, { useContext, useMemo, useState } from 'react';
import type { ToolCallMessagePartProps } from '@assistant-ui/react';
import { JsonView, darkStyles, defaultStyles } from 'react-json-view-lite';
import 'react-json-view-lite/dist/index.css';
import { UsageContext } from './UsageContext';
import {
  extractResultContent,
  getToolCallPresentation,
  isSubAgentTool,
  type ResultExtraction,
} from './toolCallPresentation';
import { MarkdownLink } from './markdownLink';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

const markdownComponents = {
  a: MarkdownLink,
};

export const ToolSummaryPreview = ({
  markdown,
  expanded,
}: {
  markdown: string;
  expanded?: boolean;
}) => (
  <div
    className={`aui-tool-summary ${
      expanded ? 'aui-tool-summary-expanded' : 'aui-tool-summary-collapsed'
    }`}
  >
    <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
      {markdown}
    </ReactMarkdown>
  </div>
);

function parseArgsInput(args: any, argsText?: string) {
  if (args) return args;
  if (!argsText) return undefined;
  try {
    return JSON.parse(argsText);
  } catch {
    return undefined;
  }
}

function resolveSummaryMarkdown(toolSummary?: string, result?: any): string | undefined {
  if (toolSummary) return toolSummary;
  if (typeof result === 'object' && typeof result?.markdown === 'string') {
    return result.markdown;
  }
  return undefined;
}

function checkIsSubAgent(
  toolName?: string,
  summaryMarkdown?: string,
  parsedArgs?: any,
  result?: any,
): boolean {
  return Boolean(
    isSubAgentTool(toolName) ||
    (typeof summaryMarkdown === 'string' && summaryMarkdown.trimStart().startsWith('@ ')) ||
    (parsedArgs && typeof parsedArgs === 'object' && ('session_id' in parsedArgs && 'message' in parsedArgs)) ||
    (typeof result === 'object' && result !== null && ('sub_agent' in result || 'sub_agent_progress' in result))
  );
}

function promptFromArgs(parsedArgs?: any): string | null {
  if (!parsedArgs || typeof parsedArgs !== 'object') {
    return null;
  }
  return typeof parsedArgs.message === 'string' ? parsedArgs.message : null;
}

function promptFromSummary(summaryMarkdown?: string): string | null {
  if (!summaryMarkdown || !summaryMarkdown.trimStart().startsWith('@ ')) {
    return null;
  }
  const newlineIdx = summaryMarkdown.indexOf('\n');
  if (newlineIdx === -1) {
    return null;
  }
  const candidate = summaryMarkdown.slice(newlineIdx + 1).trim();
  return candidate || null;
}

function extractPromptText(
  isSubAgent: boolean,
  parsedArgs?: any,
  summaryMarkdown?: string,
): string | null {
  if (!isSubAgent) return null;
  return promptFromArgs(parsedArgs) ?? promptFromSummary(summaryMarkdown);
}

function singleStringArg(args: Record<string, any>, keys: string[]): string | null {
  if (keys.length !== 1) {
    return null;
  }
  const value = args[keys[0]];
  return typeof value === 'string' ? value : null;
}

function previewFromArgs(parsedArgs: Record<string, any>): string | null {
  if (typeof parsedArgs.command === 'string') {
    return `$ ${parsedArgs.command}`;
  }
  const keys = Object.keys(parsedArgs);
  const singleString = singleStringArg(parsedArgs, keys);
  if (singleString !== null) {
    return singleString;
  }
  if (keys.length > 0) {
    return JSON.stringify(parsedArgs);
  }
  return null;
}

function getFallbackPreview(summaryMarkdown?: string, parsedArgs?: any): string | null {
  if (summaryMarkdown) {
    return null;
  }
  if (!parsedArgs || typeof parsedArgs !== 'object') {
    return null;
  }
  return previewFromArgs(parsedArgs);
}

function shouldCollapseSubAgentHeader(
  isSubAgent: boolean,
  expanded: boolean,
  promptText: string | null,
  summaryMarkdown: string,
): boolean {
  if (!isSubAgent || !expanded || !promptText) return false;
  return summaryMarkdown.trimStart().startsWith('@ ');
}

function formatHeaderSummary(
  summaryMarkdown: string | null | undefined,
  isSubAgent: boolean,
  expanded: boolean,
  promptText: string | null,
): string | null {
  if (!summaryMarkdown) return null;
  if (!shouldCollapseSubAgentHeader(isSubAgent, expanded, promptText, summaryMarkdown)) {
    return summaryMarkdown;
  }
  return summaryMarkdown.split('\n')[0] || summaryMarkdown;
}

interface ToolCallHeaderProps {
  icon: string;
  toolName: string;
  expanded: boolean;
  onToggle: () => void;
  headerSummaryMarkdown: string | null;
  fallbackPreview: string | null;
}

const ToolCallHeader: React.FC<ToolCallHeaderProps> = ({
  icon,
  toolName,
  expanded,
  onToggle,
  headerSummaryMarkdown,
  fallbackPreview,
}) => (
  <div 
    className="aui-tool-call-header" 
    onClick={onToggle}
    onKeyDown={(e) => {
      if (e.target !== e.currentTarget) return;
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onToggle();
      }
    }}
    role="button"
    tabIndex={0}
    aria-expanded={expanded}
  >
    <div className="aui-tool-call-header-content">
      <div className="aui-tool-call-header-title">
        <span className="aui-tool-call-icon">{icon}</span>
        <span className="aui-tool-call-label">
          <strong>{toolName}</strong>
        </span>
      </div>
      {headerSummaryMarkdown ? (
        <ToolSummaryPreview markdown={headerSummaryMarkdown} expanded={expanded} />
      ) : fallbackPreview ? (
        <div className={`aui-tool-summary ${expanded ? 'aui-tool-summary-expanded' : 'aui-tool-summary-collapsed'}`}>
          {fallbackPreview}
        </div>
      ) : null}
    </div>
    <div className="aui-tool-call-chevron" aria-hidden="true">
      {expanded ? '▾' : '▸'}
    </div>
  </div>
);

const ToolCallRawView: React.FC<{ argsText?: string; args: any; result: any }> = ({
  argsText,
  args,
  result,
}) => (
  <div style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
    <div>
      <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Raw Args:</div>
      <pre style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word', margin: 0, fontSize: '0.9em' }}>
        {argsText || JSON.stringify(args, null, 2)}
      </pre>
    </div>
    {result !== undefined && result !== null && (
      <div>
        <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Raw Result:</div>
        <pre style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word', margin: 0, fontSize: '0.9em' }}>
          {typeof result === 'string' ? result : JSON.stringify(result, null, 2)}
        </pre>
      </div>
    )}
  </div>
);

const ToolCallFormattedView: React.FC<{
  isSubAgent?: boolean;
  promptText?: string | null;
  parsedArgs: any;
  otherSubAgentArgs: any;
  resultContent: ResultExtraction;
  jsonStyles: any;
}> = ({
  isSubAgent,
  promptText,
  parsedArgs,
  otherSubAgentArgs,
  resultContent,
  jsonStyles,
}) => (
  <div style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
    {isSubAgent && promptText ? (
      <div>
        <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Prompt:</div>
        <div className="aui-tool-prompt-markdown">
          <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
            {promptText}
          </ReactMarkdown>
        </div>
        {otherSubAgentArgs && (
          <div style={{ marginTop: '0.5rem' }}>
            <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Parameters:</div>
            <JsonView data={otherSubAgentArgs} style={jsonStyles} />
          </div>
        )}
      </div>
    ) : (
      <div>
        <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Args:</div>
        <JsonView data={parsedArgs || {}} style={jsonStyles} />
      </div>
    )}

    {resultContent.text !== null ? (
      <div>
        <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>
          {isSubAgent ? 'Response:' : 'Result:'}
        </div>
        <div className="aui-tool-result-markdown">
          <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>
            {resultContent.text}
          </ReactMarkdown>
        </div>
      </div>
    ) : resultContent.isStructuredObject ? (
      <div>
        <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Result:</div>
        <JsonView data={resultContent.structuredData} style={jsonStyles} />
      </div>
    ) : null}
  </div>
);

export const ToolCallDetails = ({
  effectiveId,
  viewSource,
  setViewSource,
  argsText,
  args,
  parsedArgs,
  result,
  jsonStyles,
  isSubAgent,
  promptText,
}: {
  effectiveId: string;
  viewSource: boolean;
  setViewSource: (v: boolean) => void;
  argsText?: string;
  args?: any;
  parsedArgs?: any;
  result?: any;
  jsonStyles: any;
  isSubAgent?: boolean;
  promptText?: string | null;
}) => {
  const resultContent = extractResultContent(result);
  const otherSubAgentArgs = useMemo(() => {
    if (!parsedArgs || typeof parsedArgs !== 'object') return null;
    const { message: _message, ...rest } = parsedArgs;
    return Object.keys(rest).length > 0 ? rest : null;
  }, [parsedArgs]);

  return (
    <div className="aui-tool-call-body" style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <span style={{ fontSize: '0.8em', color: 'var(--text-h)' }}>
          ID: <span style={{ wordBreak: 'break-all' }}>{effectiveId}</span>
        </span>
        <button 
          type="button"
          onClick={() => setViewSource(!viewSource)}
          style={{
            fontSize: '0.8em',
            cursor: 'pointer',
            background: 'none',
            border: '1px solid var(--border)',
            borderRadius: '4px',
            padding: '2px 6px',
            color: 'var(--text)',
          }}
        >
          {viewSource ? 'Tree View' : 'View Source'}
        </button>
      </div>
      
      <div style={{ overflowX: 'auto', background: 'var(--code-bg)', padding: '0.5rem', borderRadius: '4px' }}>
        {viewSource ? (
          <ToolCallRawView argsText={argsText} args={args} result={result} />
        ) : (
          <ToolCallFormattedView
            isSubAgent={isSubAgent}
            promptText={promptText}
            parsedArgs={parsedArgs}
            otherSubAgentArgs={otherSubAgentArgs}
            resultContent={resultContent}
            jsonStyles={jsonStyles}
          />
        )}
      </div>
    </div>
  );
};

export const ToolCallCard: React.FC<ToolCallMessagePartProps> = (props) => {
  const { toolName, args, argsText, result, isError, status, toolCallId } = props as any;
  const { toolSummaries } = useContext(UsageContext);

  const effectiveId = toolCallId || (props as any).id;
  const summaryMarkdown = resolveSummaryMarkdown(toolSummaries.get(effectiveId), result);
  const parsedArgs = parseArgsInput(args, argsText);
  const isSubAgent = checkIsSubAgent(toolName, summaryMarkdown, parsedArgs, result);
  const promptText = extractPromptText(isSubAgent, parsedArgs, summaryMarkdown);

  const { icon, borderColor, defaultExpanded } = getToolCallPresentation(status, isError, {
    toolName,
    isSubAgent,
  });
  const [expanded, setExpanded] = useState(defaultExpanded);
  const [viewSource, setViewSource] = useState(false);

  const fallbackPreview = getFallbackPreview(summaryMarkdown, parsedArgs);
  const headerSummaryMarkdown = useMemo(
    () => formatHeaderSummary(summaryMarkdown, isSubAgent, expanded, promptText),
    [summaryMarkdown, isSubAgent, expanded, promptText]
  );

  const isDarkMode = typeof document !== 'undefined' && document.body.classList.contains('dark');
  const jsonStyles = isDarkMode ? darkStyles : defaultStyles;

  return (
    <div className="aui-tool-call" style={{ borderLeft: `4px solid ${borderColor}` }}>
      <ToolCallHeader
        icon={icon}
        toolName={toolName}
        expanded={expanded}
        onToggle={() => setExpanded(!expanded)}
        headerSummaryMarkdown={headerSummaryMarkdown}
        fallbackPreview={fallbackPreview}
      />

      {expanded && (
        <ToolCallDetails 
          effectiveId={effectiveId}
          viewSource={viewSource}
          setViewSource={setViewSource}
          argsText={argsText}
          args={args}
          parsedArgs={parsedArgs}
          result={result}
          jsonStyles={jsonStyles}
          isSubAgent={isSubAgent}
          promptText={promptText}
        />
      )}
    </div>
  );
};
