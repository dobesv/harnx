import React, { useMemo } from 'react';
import { AssistantRuntimeProvider } from '@assistant-ui/react';
import { useAgUiRuntime } from '@assistant-ui/react-ag-ui';
import { HttpAgent } from '@ag-ui/client';

export interface ChatProviderProps {
  agentName: string;
  sessionId: string;
  children: React.ReactNode;
}

export const ChatProvider: React.FC<ChatProviderProps> = ({ agentName, sessionId, children }) => {
  const agent = useMemo(() => {
    return new HttpAgent({ url: `/v1/agents/${encodeURIComponent(agentName)}/sessions/${encodeURIComponent(sessionId)}` });
  }, [agentName, sessionId]);

  const runtime = useAgUiRuntime({ agent });

  return (
    <AssistantRuntimeProvider key={`${agentName}:${sessionId}`} runtime={runtime}>
      {children}
    </AssistantRuntimeProvider>
  );
};
