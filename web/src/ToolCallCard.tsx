import React, { useContext, useState } from 'react';
import type { ToolCallMessagePartProps } from '@assistant-ui/react';
import { JsonView, darkStyles, defaultStyles } from 'react-json-view-lite';
import 'react-json-view-lite/dist/index.css';
import { UsageContext } from './UsageContext';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

const ToolSummaryPreview = ({ markdown }: { markdown: string }) => (
  <div className="aui-tool-summary" style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
    <ReactMarkdown 
      remarkPlugins={[remarkGfm]}
      components={{
        p: ({ children }: any) => <span style={{ display: 'inline' }}>{children} </span>,
        h1: ({ children }: any) => <span style={{ display: 'inline', fontWeight: 'bold' }}>{children} </span>,
        h2: ({ children }: any) => <span style={{ display: 'inline', fontWeight: 'bold' }}>{children} </span>,
        h3: ({ children }: any) => <span style={{ display: 'inline', fontWeight: 'bold' }}>{children} </span>,
        h4: ({ children }: any) => <span style={{ display: 'inline', fontWeight: 'bold' }}>{children} </span>,
        h5: ({ children }: any) => <span style={{ display: 'inline', fontWeight: 'bold' }}>{children} </span>,
        h6: ({ children }: any) => <span style={{ display: 'inline', fontWeight: 'bold' }}>{children} </span>,
        ul: ({ children }: any) => <span style={{ display: 'inline' }}>{children} </span>,
        ol: ({ children }: any) => <span style={{ display: 'inline' }}>{children} </span>,
        li: ({ children }: any) => <span style={{ display: 'inline' }}>• {children} </span>,
        pre: ({ children }: any) => <span style={{ display: 'inline' }}>{children} </span>,
        code: ({ children }: any) => <span style={{ display: 'inline', fontFamily: 'monospace' }}>{children}</span>,
        blockquote: ({ children }: any) => <span style={{ display: 'inline' }}>{children} </span>
      }}
    >
      {markdown}
    </ReactMarkdown>
  </div>
);

const ToolCallDetails = ({
  effectiveId,
  viewSource,
  setViewSource,
  argsText,
  args,
  parsedArgs,
  result,
  jsonStyles
}: {
  effectiveId: string,
  viewSource: boolean,
  setViewSource: (v: boolean) => void,
  argsText: string,
  args: any,
  parsedArgs: any,
  result: any,
  jsonStyles: any
}) => (
  <div className="aui-tool-call-body" style={{ display: 'flex', flexDirection: 'column', gap: '0.5rem' }}>
    <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
      <span style={{ fontSize: '0.8em', color: 'var(--text-h)' }}>ID: <span style={{ wordBreak: 'break-all' }}>{effectiveId}</span></span>
      <button 
        onClick={() => setViewSource(!viewSource)}
        style={{ fontSize: '0.8em', cursor: 'pointer', background: 'none', border: '1px solid var(--border)', borderRadius: '4px', padding: '2px 6px' }}
      >
        {viewSource ? 'Tree View' : 'View Source'}
      </button>
    </div>
    
    <div style={{ overflowX: 'auto', background: 'var(--code-bg)', padding: '0.5rem', borderRadius: '4px' }}>
      {viewSource ? (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
          <div>
            <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Raw Args:</div>
            <pre style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word', margin: 0, fontSize: '0.9em' }}>
              {argsText || JSON.stringify(args, null, 2)}
            </pre>
          </div>
          {result && (
            <div>
              <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Raw Result:</div>
              <pre style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word', margin: 0, fontSize: '0.9em' }}>
                {typeof result === 'string' ? result : JSON.stringify(result, null, 2)}
              </pre>
            </div>
          )}
        </div>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '1rem' }}>
          <div>
            <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Args:</div>
            <JsonView data={parsedArgs || {}} style={jsonStyles} />
          </div>
          {result && (
            <div>
              <div style={{ fontSize: '0.8em', marginBottom: '0.25rem', fontWeight: 'bold' }}>Result:</div>
              <JsonView data={typeof result === 'string' ? { result } : result} style={jsonStyles} />
            </div>
          )}
        </div>
      )}
    </div>
  </div>
);

export const ToolCallCard: React.FC<ToolCallMessagePartProps> = (props) => {
  const { toolName, args, argsText, result, isError, status, toolCallId } = props as any;
  const { toolSummaries } = useContext(UsageContext);

  const statusType = status?.type;
  
  let borderColor = 'var(--border)';
  if (statusType === 'running') borderColor = 'var(--status-running)';
  else if (statusType === 'complete') borderColor = 'var(--status-complete)';
  else if (statusType === 'requires-action') borderColor = 'var(--status-action)';
  else if (statusType === 'incomplete' || isError) borderColor = 'var(--status-error)';

  const [expanded, setExpanded] = useState(statusType === 'requires-action');
  const [viewSource, setViewSource] = useState(false);

  // Get markdown summary
  const effectiveId = toolCallId || (props as any).id;
  const summaryMarkdown = toolSummaries.get(effectiveId) || 
    (typeof result === 'object' && result?.markdown) ||
    (typeof result === 'string' && result);

  let parsedArgs = args;
  if (!parsedArgs && argsText) {
    try {
      parsedArgs = JSON.parse(argsText);
    } catch {
      // ignore
    }
  }

  const isDarkMode = typeof document !== 'undefined' && document.body.classList.contains('dark');
  const jsonStyles = isDarkMode ? darkStyles : defaultStyles;

  return (
    <div className="aui-tool-call" style={{ borderLeft: `4px solid ${borderColor}` }}>
      <div 
        className="aui-tool-call-header" 
        onClick={() => setExpanded(!expanded)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setExpanded(!expanded);
          }
        }}
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        style={{ cursor: 'pointer', display: 'flex', justifyContent: 'space-between' }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: '0.5rem', overflow: 'hidden' }}>
          <span className="aui-tool-call-icon">
            {statusType === 'running' ? '⏳' : statusType === 'requires-action' ? '⚠️' : isError ? '❌' : '✅'}
          </span>
          <span className="aui-tool-call-label">
            <strong>{toolName}</strong>
          </span>
          {summaryMarkdown ? <ToolSummaryPreview markdown={summaryMarkdown} /> : null}
        </div>
        <div>
          {expanded ? '▾' : '▸'}
        </div>
      </div>

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
        />
      )}
    </div>
  );
};