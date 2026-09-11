import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createConnectionCoordinator } from '../connection';

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(10_000);
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
});

describe('connection coordinator', () => {
  it('publishes immutable snapshots only when visible state changes', () => {
    const coordinator = createConnectionCoordinator();
    const listener = vi.fn();
    const unsubscribe = coordinator.subscribe(listener);
    const initial = coordinator.getSnapshot();
    expect(initial).toEqual({ status: 'unknown', attempt: 0, nextRetryAt: null, retryingReads: 0 });
    expect(Object.isFrozen(initial)).toBe(true);
    expect(coordinator.getSnapshot()).toBe(initial);

    coordinator.noteTransientTrouble();
    const degraded = coordinator.getSnapshot();
    expect(degraded.status).toBe('degraded');
    expect(degraded).not.toBe(initial);
    expect(Object.isFrozen(degraded)).toBe(true);
    coordinator.noteTransientTrouble();
    expect(coordinator.getSnapshot()).toBe(degraded);
    expect(listener).toHaveBeenCalledTimes(1);

    coordinator.noteSuccess();
    const online = coordinator.getSnapshot();
    expect(online.status).toBe('online');
    coordinator.noteSuccess();
    expect(coordinator.getSnapshot()).toBe(online);
    expect(listener).toHaveBeenCalledTimes(2);
    unsubscribe();
    coordinator.noteTransientTrouble();
    expect(listener).toHaveBeenCalledTimes(2);
    expect(initial.status).toBe('unknown');
  });

  it('joins later failures to one jittered deadline and retains in-flight registrations', async () => {
    const random = vi.fn(() => 0.5);
    const coordinator = createConnectionCoordinator({ random });
    const first = coordinator.waitForRetry(new AbortController().signal);
    expect(coordinator.getSnapshot()).toEqual({
      status: 'reconnecting', attempt: 1, nextRetryAt: 11_000, retryingReads: 1,
    });
    await vi.advanceTimersByTimeAsync(250);
    const second = coordinator.waitForRetry(new AbortController().signal);
    expect(coordinator.getSnapshot()).toEqual({
      status: 'reconnecting', attempt: 1, nextRetryAt: 11_000, retryingReads: 2,
    });
    expect(random).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(1);

    await vi.advanceTimersByTimeAsync(750);
    const [unregisterFirst, unregisterSecond] = await Promise.all([first, second]);
    expect(coordinator.getSnapshot()).toEqual({
      status: 'reconnecting', attempt: 1, nextRetryAt: null, retryingReads: 2,
    });
    expect(vi.getTimerCount()).toBe(0);
    coordinator.noteSuccess();
    unregisterFirst();
    unregisterFirst();
    expect(coordinator.getSnapshot().retryingReads).toBe(1);
    expect(coordinator.getSnapshot().status).toBe('reconnecting');
    unregisterSecond();
    expect(coordinator.getSnapshot()).toEqual({
      status: 'online', attempt: 0, nextRetryAt: null, retryingReads: 0,
    });
  });

  it('starts another round without waiting for a slow retry and never double-counts a read', async () => {
    const coordinator = createConnectionCoordinator({ random: () => 0.5 });
    const fast = new AbortController();
    const first = coordinator.waitForRetry(fast.signal);
    const slow = coordinator.waitForRetry(new AbortController().signal);
    expect(coordinator.waitForRetry(fast.signal)).toBe(first);
    await vi.advanceTimersByTimeAsync(1_000);
    const unregisterFast = await first;
    const unregisterSlow = await slow;

    const second = coordinator.waitForRetry(fast.signal);
    expect(coordinator.getSnapshot()).toEqual({
      status: 'reconnecting', attempt: 2, nextRetryAt: 13_000, retryingReads: 2,
    });
    coordinator.noteSuccess();
    unregisterSlow();
    expect(coordinator.getSnapshot().status).toBe('reconnecting');
    expect(coordinator.getSnapshot().nextRetryAt).toBe(13_000);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(await second).toBe(unregisterFast);
    coordinator.noteSuccess();
    unregisterFast();
    expect(coordinator.getSnapshot().attempt).toBe(0);
  });

  it.each([0, 0.5, 0.999])('caps exponential backoff before applying jitter (%s)', async (randomValue) => {
    const coordinator = createConnectionCoordinator({ random: () => randomValue });
    const controller = new AbortController();
    let unregister: (() => void) | undefined;
    for (let round = 1; round <= 20; round += 1) {
      const pending = coordinator.waitForRetry(controller.signal);
      const baseMs = Math.min(60_000, 1_000 * 2 ** Math.min(round - 1, 6));
      const delayMs = baseMs * (0.5 + randomValue);
      expect(coordinator.getSnapshot()).toEqual({
        status: 'reconnecting', attempt: round, nextRetryAt: Date.now() + delayMs, retryingReads: 1,
      });
      await vi.advanceTimersByTimeAsync(Math.ceil(delayMs));
      unregister = await pending;
    }
    unregister?.();
    expect(coordinator.getSnapshot().status).toBe('degraded');
    expect(vi.getTimerCount()).toBe(0);
  });

  it('rejects an already-aborted signal without observing trouble or scheduling a timer', async () => {
    const coordinator = createConnectionCoordinator();
    const initial = coordinator.getSnapshot();
    const controller = new AbortController();
    controller.abort('navigation');
    await expect(coordinator.waitForRetry(controller.signal)).rejects.toMatchObject({ name: 'AbortError' });
    expect(coordinator.getSnapshot()).toBe(initial);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('removes aborted waiters and their listeners, leaving other waiters on the same deadline', async () => {
    const coordinator = createConnectionCoordinator({ random: () => 0.5 });
    const first = new AbortController();
    const second = new AbortController();
    const removeListener = vi.spyOn(first.signal, 'removeEventListener');
    const firstWait = coordinator.waitForRetry(first.signal);
    const secondWait = coordinator.waitForRetry(second.signal);
    const firstRejection = expect(firstWait).rejects.toMatchObject({ name: 'AbortError' });
    first.abort('navigation');
    await firstRejection;
    expect(removeListener).toHaveBeenCalledWith('abort', expect.any(Function));
    expect(coordinator.getSnapshot()).toEqual({
      status: 'reconnecting', attempt: 1, nextRetryAt: 11_000, retryingReads: 1,
    });
    expect(vi.getTimerCount()).toBe(1);

    const secondRejection = expect(secondWait).rejects.toMatchObject({ name: 'AbortError' });
    second.abort();
    await secondRejection;
    expect(coordinator.getSnapshot()).toEqual({
      status: 'degraded', attempt: 0, nextRetryAt: null, retryingReads: 0,
    });
    expect(vi.getTimerCount()).toBe(0);
  });

  it('clears an empty wait queue without resetting rounds while another retry is in flight', async () => {
    const coordinator = createConnectionCoordinator({ random: () => 0.5 });
    const slow = new AbortController();
    const fast = new AbortController();
    const slowWait = coordinator.waitForRetry(slow.signal);
    const fastWait = coordinator.waitForRetry(fast.signal);
    await vi.advanceTimersByTimeAsync(1_000);
    const unregisterSlow = await slowWait;
    const unregisterFast = await fastWait;
    const nextWait = coordinator.waitForRetry(fast.signal);
    const rejection = expect(nextWait).rejects.toMatchObject({ name: 'AbortError' });
    fast.abort();
    await rejection;
    unregisterFast();
    expect(coordinator.getSnapshot()).toEqual({
      status: 'reconnecting', attempt: 2, nextRetryAt: null, retryingReads: 1,
    });
    expect(vi.getTimerCount()).toBe(0);

    slow.abort();
    unregisterSlow();
    expect(coordinator.getSnapshot()).toEqual({
      status: 'degraded', attempt: 0, nextRetryAt: null, retryingReads: 0,
    });
  });

  it('keeps the latest interaction as evidence after the final cleanup, regardless of call order', async () => {
    const coordinator = createConnectionCoordinator({ random: () => 0.5 });
    const controller = new AbortController();
    let waiting = coordinator.waitForRetry(controller.signal);
    const reconnecting = coordinator.getSnapshot();
    coordinator.noteSuccess();
    coordinator.noteTransientTrouble();
    expect(coordinator.getSnapshot()).toBe(reconnecting);
    await vi.advanceTimersByTimeAsync(1_000);
    (await waiting)();
    expect(coordinator.getSnapshot().status).toBe('degraded');
    coordinator.noteSuccess();
    expect(coordinator.getSnapshot().status).toBe('online');

    waiting = coordinator.waitForRetry(controller.signal);
    await vi.advanceTimersByTimeAsync(1_000);
    coordinator.noteSuccess();
    expect(coordinator.getSnapshot().status).toBe('reconnecting');
    (await waiting)();
    expect(coordinator.getSnapshot().status).toBe('online');
  });

  it('does not let stale cleanup remove a newer registration with the same signal', async () => {
    const coordinator = createConnectionCoordinator({ random: () => 0.5 });
    const controller = new AbortController();
    const first = coordinator.waitForRetry(controller.signal);
    await vi.advanceTimersByTimeAsync(1_000);
    const unregisterFirst = await first;
    unregisterFirst();
    const second = coordinator.waitForRetry(controller.signal);
    unregisterFirst();
    expect(coordinator.getSnapshot().retryingReads).toBe(1);
    await vi.advanceTimersByTimeAsync(1_000);
    (await second)();
    expect(coordinator.getSnapshot().retryingReads).toBe(0);
  });

  it('supports injected clocks and numeric timer handles, including zero', async () => {
    let fire: () => void = () => {};
    const setTimeout = vi.fn((callback: () => void) => { fire = callback; return 0; });
    const clearTimeout = vi.fn();
    const coordinator = createConnectionCoordinator({
      now: () => 123, random: () => 0.25, setTimeout, clearTimeout, initialDelayMs: 20, maxDelayMs: 30,
    });
    const controller = new AbortController();
    const first = coordinator.waitForRetry(controller.signal);
    expect(setTimeout).toHaveBeenCalledWith(expect.any(Function), 15);
    expect(coordinator.getSnapshot().nextRetryAt).toBe(138);
    fire();
    const unregister = await first;
    const second = coordinator.waitForRetry(controller.signal);
    expect(setTimeout).toHaveBeenLastCalledWith(expect.any(Function), 22.5);
    const rejection = expect(second).rejects.toMatchObject({ name: 'AbortError' });
    unregister();
    await rejection;
    expect(clearTimeout).toHaveBeenCalledWith(0);
    expect(coordinator.getSnapshot().attempt).toBe(0);
  });
});

describe('connection singleton overrides', () => {
  it.each([true, false])('honors delay overrides only when DEV is true (DEV=%s)', async (dev) => {
    vi.stubEnv('DEV', dev);
    vi.spyOn(Math, 'random').mockReturnValue(0.5);
    vi.stubGlobal('window', { __harnxConnection: { initialDelayMs: 10, maxDelayMs: 15 } });
    vi.resetModules();
    const { connection } = await import('../connection');
    const controller = new AbortController();
    const first = connection.waitForRetry(controller.signal);
    const firstDelay = dev ? 10 : 1_000;
    expect(connection.getSnapshot().nextRetryAt).toBe(Date.now() + firstDelay);
    await vi.advanceTimersByTimeAsync(firstDelay);
    const unregister = await first;
    const second = connection.waitForRetry(controller.signal);
    expect(connection.getSnapshot().nextRetryAt).toBe(Date.now() + (dev ? 15 : 2_000));
    const rejection = expect(second).rejects.toMatchObject({ name: 'AbortError' });
    controller.abort();
    await rejection;
    unregister();
    expect(vi.getTimerCount()).toBe(0);
  });
});
