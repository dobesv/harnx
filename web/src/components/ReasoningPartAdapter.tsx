import React from 'react';
import { useAuiState } from '@assistant-ui/react';
import { ThinkingBlock } from './ThinkingBlock';

export const ReasoningPartAdapter: React.FC<{
  text?: string;
  status?: { type?: string };
}> = (props) => {
  const storePart = useAuiState((s) => s?.part);
  const partText = storePart && 'text' in storePart ? (storePart.text as string) : '';
  const text = props.text !== undefined ? props.text : partText;

  const partIsRunning = (props.status ?? storePart?.status)?.type === 'running';

  return <ThinkingBlock text={text} isStreaming={partIsRunning} />;
};
