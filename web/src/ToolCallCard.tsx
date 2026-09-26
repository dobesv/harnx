import React, { useContext, useEffect, useMemo, useRef, useState } from 'react';
import type { ToolCallMessagePartProps } from '@assistant-ui/react';
import { JsonView, darkStyles, defaultStyles } from 'react-json-view-lite';
import 'react-json-view-lite/dist/index.css';
import { UsageContext } from './UsageContext';
import type { ToolCallLocation, ToolCallState } from './toolUpdates';
import {
  classifyToolCallStatus,
  extractResultContent,
  formatElapsedMs,
  getToolCallPresentation,
  isSubAgentTool,
  TOOL_TIMER_MIN_ELAPSED_MS,
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

function resolveSummaryMarkdown(toolSummary?: string, result?: any, liveMarkdown?: string): string | undefined {
  if (liveMarkdown) return liveMarkdown;
  if (toolSummary) return toolSummary;
  if (typeof result === 'object' && typeof result?.markdown === 'string') {
    return result.markdown;
  }
  return undefined;
}

// Normalize a tool result to its object form, parsing JSON-string results so
// serialized sub-agent markers are detected the same as raw-object ones. Mirrors
// resultValue() in subAgentNotes.ts, which the sub-agent note pipeline relies on.
function resultObject(result: unknown): Record<string, any> | null {
  if (typeof result === 'string') {
    try {
      const parsed = JSON.parse(result);
      return parsed && typeof parsed === 'object' ? parsed : null;
    } catch {
      return null;
    }
  }
  return result && typeof result === 'object' ? (result as Record<string, any>) : null;
}

function hasSubAgentMarker(result?: unknown): boolean {
  const obj = resultObject(result);
  return obj !== null && ('sub_agent' in obj || 'sub_agent_progress' in obj);
}

function hasSubAgentArgs(parsedArgs?: any): boolean {
  return Boolean(
    parsedArgs && typeof parsedArgs === 'object' && 'session_id' in parsedArgs && 'message' in parsedArgs,
  );
}

function checkIsSubAgent(
  toolName?: string,
  summaryMarkdown?: string,
  parsedArgs?: any,
  result?: any,
): boolean {
  if (isSubAgentTool(toolName)) return true;
  if (typeof summaryMarkdown === 'string' && summaryMarkdown.trimStart().startsWith('@ ')) return true;
  if (hasSubAgentArgs(parsedArgs)) return true;
  return hasSubAgentMarker(result);
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

// ─── Sub-agent prompt deduplication invariant (#1911) ─────────────────────────────
// Sub-agent prompt text can surface through three independent rendering paths:
// 1. Body "Prompt:" section (ToolCallFormattedView, when isSubAgent && promptText)
// 2. Header server-summary (formatHeaderSummary collapses '@ '-prefixed summaries)
// 3. Header args-fallback (previewFromArgs when no summary available)
// Invariant: prompt must render once. Body (path 1) is primary. Paths 2 and 3 must
// suppress prompt text when body already shows it:
// - shouldCollapseSubAgentHeader: suppresses path 2 when expanded and '@ ' summary
// - getFallbackPreview: suppresses path 3 when isSubAgent && promptText
// Exception: session_new tools (no message arg) have promptText=null, so fallback OK.
// ────────────────────────────────────────────────────────────────────────────────

function getFallbackPreview(
  summaryMarkdown?: string,
  parsedArgs?: any,
  isSubAgent?: boolean,
  promptText?: string | null,
): string | null {
  if (summaryMarkdown) {
    return null;
  }
  if (isSubAgent && promptText) {
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
  title?: string | null;
  expanded: boolean;
  onToggle: () => void;
  headerSummaryMarkdown: string | null;
  fallbackPreview: React.ReactNode;
  elapsedText?: string | null;
  locations?: ToolCallLocation[];
}

function formatLocation(location: ToolCallLocation): string {
  const fileName = location.path.split(/[/\\]/).pop() || location.path;
  return location.line !== undefined ? `${fileName}:${location.line}` : fileName;
}

const ToolCallLocations: React.FC<{ locations?: ToolCallLocation[] }> = ({ locations }) => {
  if (!locations?.length) return null;
  return (
    <span
      className="aui-tool-call-locations"
      style={{ marginLeft: '0.5em', fontWeight: 'normal', opacity: 0.7 }}
    >
      {locations.slice(0, 3).map(formatLocation).join(', ')}
      {locations.length > 3 && ` +${locations.length - 3} more`}
    </span>
  );
};

interface ToolCallSummaryContentProps {
  headerSummaryMarkdown?: string | null;
  fallbackPreview?: React.ReactNode;
  expanded: boolean;
}

const ToolCallSummaryContent: React.FC<ToolCallSummaryContentProps> = ({
  headerSummaryMarkdown,
  fallbackPreview,
  expanded,
}) => {
  if (headerSummaryMarkdown) {
    return <ToolSummaryPreview markdown={headerSummaryMarkdown} expanded={expanded} />;
  }
  if (!fallbackPreview) return null;
  const stateClass = expanded ? 'aui-tool-summary-expanded' : 'aui-tool-summary-collapsed';
  return <div className={`aui-tool-summary ${stateClass}`}>{fallbackPreview}</div>;
};

interface ToolCallHeaderTitleProps {
  icon: string;
  toolName: string;
  title?: string | null;
  elapsedText?: string | null;
  locations?: ToolCallLocation[];
}

const ToolCallHeaderTitle: React.FC<ToolCallHeaderTitleProps> = ({
  icon,
  toolName,
  title,
  elapsedText,
  locations,
}) => (
  <div className="aui-tool-call-header-title">
    <span className="aui-tool-call-icon">{icon}</span>
    <span className="aui-tool-call-label">
      <strong>{title || toolName}</strong>
      {elapsedText && <span className="aui-tool-call-elapsed"> ({elapsedText})</span>}
      <ToolCallLocations locations={locations} />
    </span>
  </div>
);

function handleToolCallHeaderKeyDown(
  event: React.KeyboardEvent<HTMLDivElement>,
  onToggle: () => void,
): void {
  if (event.target !== event.currentTarget) return;
  if (event.key !== 'Enter' && event.key !== ' ') return;
  event.preventDefault();
  onToggle();
}

const ToolCallHeader: React.FC<ToolCallHeaderProps> = ({
  icon,
  toolName,
  title,
  expanded,
  onToggle,
  headerSummaryMarkdown,
  fallbackPreview,
  elapsedText,
  locations,
}) => (
  <div
    className="aui-tool-call-header"
    onClick={onToggle}
    onKeyDown={(event) => handleToolCallHeaderKeyDown(event, onToggle)}
    role="button"
    tabIndex={0}
    aria-expanded={expanded}
  >
    <div className="aui-tool-call-header-content">
      <ToolCallHeaderTitle
        icon={icon}
        toolName={toolName}
        title={title}
        elapsedText={elapsedText}
        locations={locations}
      />
      <ToolCallSummaryContent
        headerSummaryMarkdown={headerSummaryMarkdown}
        fallbackPreview={fallbackPreview}
        expanded={expanded}
      />
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

interface ResolvedToolContent {
  summaryMarkdown?: string;
  parsedArgs: any;
  isSubAgent: boolean;
  promptText: string | null;
}

interface ResolveToolContentOptions {
  toolName?: string;
  args: any;
  argsText?: string;
  result: any;
  storedSummary?: string;
  liveMarkdown?: string;
}

function resolveToolContent({
  toolName,
  args,
  argsText,
  result,
  storedSummary,
  liveMarkdown,
}: ResolveToolContentOptions): ResolvedToolContent {
  const summaryMarkdown = resolveSummaryMarkdown(storedSummary, result, liveMarkdown);
  const parsedArgs = parseArgsInput(args, argsText);
  const isSubAgent = checkIsSubAgent(toolName, summaryMarkdown, parsedArgs, result);
  return {
    summaryMarkdown,
    parsedArgs,
    isSubAgent,
    promptText: extractPromptText(isSubAgent, parsedArgs, summaryMarkdown),
  };
}

interface ComputeToolPresentationOptions {
  status: any;
  isError: boolean | undefined;
  toolName: string | undefined;
  isSubAgent: boolean;
  liveUpdate?: ToolCallState;
}

function computeToolPresentation({
  status,
  isError,
  toolName,
  isSubAgent,
  liveUpdate,
}: ComputeToolPresentationOptions) {
  return getToolCallPresentation(status, isError, {
    toolName,
    isSubAgent,
    kind: liveUpdate?.kind,
  });
}

function currentElapsedMs(
  isPending: boolean,
  clockMs: number,
  startedAtMs: number | null,
  finalElapsedMs: number | null,
): number {
  if (!isPending) return finalElapsedMs ?? 0;
  return startedAtMs === null ? 0 : Math.max(0, clockMs - startedAtMs);
}

function elapsedTextFor(elapsedMs: number, suppressTimer: boolean): string | null {
  return !suppressTimer && elapsedMs >= TOOL_TIMER_MIN_ELAPSED_MS
    ? formatElapsedMs(elapsedMs)
    : null;
}

interface MutableTimeRef {
  current: number | null;
}

function captureStartTime(isPending: boolean, startedAtMsRef: MutableTimeRef): void {
  if (isPending && startedAtMsRef.current === null) {
    startedAtMsRef.current = Date.now();
  }
}

function freezeFinalElapsed(
  isPending: boolean,
  startedAtMsRef: MutableTimeRef,
  finalElapsedMsRef: MutableTimeRef,
): void {
  if (!isPending && startedAtMsRef.current !== null && finalElapsedMsRef.current === null) {
    finalElapsedMsRef.current = Math.max(0, Date.now() - startedAtMsRef.current);
  }
}

function useTimerClock(isPending: boolean, suppressTimer: boolean): number {
  const [clockMs, setClockMs] = useState(0);
  useEffect(() => {
    if (!isPending || suppressTimer) return undefined;
    const timer = window.setInterval(() => setClockMs(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [isPending, suppressTimer]);
  return clockMs;
}

function useToolElapsedText(status: any, toolName?: string): string | null {
  // Name-based suppression avoids hiding timers for ordinary tools that happen
  // to have session_id/message arguments.
  const isPending = classifyToolCallStatus(status).isPending;
  const suppressTimer = isSubAgentTool(toolName);
  const startedAtMsRef = useRef<number | null>(null);
  const finalElapsedMsRef = useRef<number | null>(null);
  const clockMs = useTimerClock(isPending, suppressTimer);

  // Client-side anchor may under-count after late hydration or reconnect.
  captureStartTime(isPending, startedAtMsRef);
  freezeFinalElapsed(isPending, startedAtMsRef, finalElapsedMsRef);

  const elapsedMs = currentElapsedMs(
    isPending,
    clockMs,
    startedAtMsRef.current,
    finalElapsedMsRef.current,
  );
  return elapsedTextFor(elapsedMs, suppressTimer);
}

interface ResolveToolHeaderPropsOptions {
  icon: string;
  toolName: string;
  title?: string;
  expanded: boolean;
  onToggle: () => void;
  summaryMarkdown?: string;
  parsedArgs: any;
  isSubAgent: boolean;
  promptText: string | null;
  elapsedText: string | null;
  locations?: { path: string; line?: number }[];
}

function resolveToolHeaderProps({
  icon,
  toolName,
  title,
  expanded,
  onToggle,
  summaryMarkdown,
  parsedArgs,
  isSubAgent,
  promptText,
  elapsedText,
  locations,
}: ResolveToolHeaderPropsOptions): ToolCallHeaderProps {
  return {
    icon,
    toolName,
    title,
    expanded,
    onToggle,
    headerSummaryMarkdown: formatHeaderSummary(
      summaryMarkdown,
      isSubAgent,
      expanded,
      promptText,
    ),
    fallbackPreview: getFallbackPreview(
      summaryMarkdown,
      parsedArgs,
      isSubAgent,
      promptText,
    ),
    elapsedText,
    locations,
  };
}

export const ToolCallCard: React.FC<ToolCallMessagePartProps> = (props) => {
  const { toolName, args, argsText, result, isError, status, toolCallId } = props as any;
  const { toolSummaries, toolUpdates } = useContext(UsageContext);
  const effectiveId = toolCallId || (props as any).id;
  const liveUpdate = toolUpdates.get(effectiveId);
  const content = resolveToolContent({
    toolName,
    args,
    argsText,
    result,
    storedSummary: toolSummaries.get(effectiveId),
    liveMarkdown: liveUpdate?.markdown,
  });
  const { icon, borderColor, defaultExpanded } = computeToolPresentation({
    status,
    isError,
    toolName,
    isSubAgent: content.isSubAgent,
    liveUpdate,
  });
  const [expanded, setExpanded] = useState(defaultExpanded);
  const [viewSource, setViewSource] = useState(false);
  const elapsedText = useToolElapsedText(status, toolName);
  const headerProps = resolveToolHeaderProps({
    icon,
    toolName,
    title: liveUpdate?.title,
    expanded,
    onToggle: () => setExpanded(!expanded),
    ...content,
    elapsedText,
    locations: liveUpdate?.locations,
  });
  const isDarkMode = typeof document !== 'undefined' && document.body.classList.contains('dark');
  const jsonStyles = isDarkMode ? darkStyles : defaultStyles;

  return (
    <div className="aui-tool-call" style={{ borderLeft: `4px solid ${borderColor}` }}>
      <ToolCallHeader {...headerProps} />

      {expanded && (
        <ToolCallDetails
          effectiveId={effectiveId}
          viewSource={viewSource}
          setViewSource={setViewSource}
          argsText={argsText}
          args={args}
          parsedArgs={content.parsedArgs}
          result={result}
          jsonStyles={jsonStyles}
          isSubAgent={content.isSubAgent}
          promptText={content.promptText}
        />
      )}
    </div>
  );
};
