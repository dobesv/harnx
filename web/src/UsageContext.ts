import { createContext } from 'react';

export interface UsageData {
  input: number;
  output: number;
  cached?: number;
  session_label?: string;
  context_tokens?: number;
  max_context_tokens?: number | null;
  context_percent?: number;
}

export const UsageContext = createContext<{
  usage: UsageData | null;
  toolSummaries: Map<string, string>;
}>({
  usage: null,
  toolSummaries: new Map(),
});
