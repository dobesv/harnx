import { createContext } from 'react';
import type { SubAgentNote } from './subAgentNotes';

interface SubAgentNotesContextValue {
  notes: SubAgentNote[];
  openSession: (agent: string, sessionId: string) => void;
}

export const SubAgentNotesContext = createContext<SubAgentNotesContextValue>({
  notes: [],
  openSession: () => {},
});
