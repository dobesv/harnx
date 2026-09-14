import { afterEach, describe, expect, it } from 'vitest';
import { handleHarnxCustomEvent, type HarnxCustomEventCallbacks } from './harnxCustomEvents';

const callbacks = (isForeground: boolean): HarnxCustomEventCallbacks => ({
  onStatus: () => {},
  onRunFailed: () => {},
  onUsage: () => {},
  onToolSummary: () => {},
  isForeground,
});

describe('harnx custom session title events', () => {
  afterEach(() => {
    document.title = '';
  });

  it('ignores title events from child observers', () => {
    document.title = 'existing title';

    handleHarnxCustomEvent(
      'session_title_updated',
      { title: 'child title' },
      callbacks(false),
    );

    expect(document.title).toBe('existing title');
  });

  it('updates title for the foreground session', () => {
    handleHarnxCustomEvent(
      'session_title_updated',
      { title: 'foreground title' },
      callbacks(true),
    );

    expect(document.title).toBe('harnx — foreground title');
  });
});
