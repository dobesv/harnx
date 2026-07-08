import { setupWorker } from 'msw/browser';
import { handlers, scenarios } from './handlers';

// Check if a scenario is requested via URL
const urlParams = new URLSearchParams(window.location.search);
const scenarioName = urlParams.get('scenario');
const initialHandlers = scenarioName && (scenarios as any)[scenarioName] 
  ? (scenarios as any)[scenarioName] 
  : handlers;

export const worker = setupWorker(...initialHandlers);

if (typeof window !== 'undefined') {
  (window as any).__msw = { worker, scenarios };
}
