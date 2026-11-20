import { createContext, useContext } from 'react';
import type { A2uiSurface, A2uiSurfaceResolution } from './surfaceState';

export interface A2uiSurfacesContextValue {
  agentName: string;
  sessionId: string;
  surfaces: Map<string, A2uiSurface>;
  getSurfacesForToolCall: (toolCallId: string) => A2uiSurface[];
  unattachedSurfaces: A2uiSurface[];
  updateSurfaceResolution: (
    surfaceId: string,
    publishSeq: number,
    resolution: A2uiSurfaceResolution
  ) => void;
}

export const A2uiSurfacesContext = createContext<A2uiSurfacesContextValue>({
  agentName: '',
  sessionId: '',
  surfaces: new Map(),
  getSurfacesForToolCall: () => [],
  unattachedSurfaces: [],
  updateSurfaceResolution: () => {},
});

export function useA2uiSurfaces(): A2uiSurfacesContextValue {
  return useContext(A2uiSurfacesContext);
}
