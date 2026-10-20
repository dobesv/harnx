import { createContext } from 'react';

export type SessionNavigateFn = (agent: string, sessionId: string) => void;

export interface SessionNavigationContextValue {
  navigateSession: SessionNavigateFn;
}

export const SessionNavigationContext = createContext<SessionNavigationContextValue>({
  navigateSession: () => {},
});
