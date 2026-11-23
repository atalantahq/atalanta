package com.facebook.react.modules.core;

import java.util.PriorityQueue;

/**
 * Host-check fake mirroring RN 0.77/0.81 {@code JavaTimerManager.kt}:
 * {@code private val timers: PriorityQueue<Timer>} guarded by
 * {@code private val timerGuard}, {@code private class Timer(timerId,
 * targetTime, interval, repeat)}, and {@code internal fun
 * hasActiveTimersInRange}, which Kotlin compiles under the mangled name
 * {@code hasActiveTimersInRange$ReactAndroid_release}.
 */
public class JavaTimerManager {
    private static final class Timer {
        final int timerId;
        long targetTime;
        final int interval;
        final boolean repeat;

        Timer(int timerId, long targetTime, int interval, boolean repeat) {
            this.timerId = timerId;
            this.targetTime = targetTime;
            this.interval = interval;
            this.repeat = repeat;
        }
    }

    private final Object timerGuard = new Object();
    private PriorityQueue<Timer> timers = new PriorityQueue<>(11, (a, b) -> Long.signum(a.targetTime - b.targetTime));
    public long lastRange = -1;

    /** RN: createTimer(timerId, delay, repeat), target = now + delay (now passed in: the checks' clock). */
    public void createTimer(int timerId, long now, long delay, boolean repeat) {
        synchronized (timerGuard) {
            timers.add(new Timer(timerId, now + delay, (int) delay, repeat));
        }
    }

    public void deleteTimer(int timerId) {
        synchronized (timerGuard) {
            timers.removeIf(t -> t.timerId == timerId);
        }
    }

    /** The frame callback: drops (fires) one-shot timers due before {@code now}; returns their ids. */
    public java.util.List<Integer> fireDue(long now) {
        java.util.List<Integer> out = new java.util.ArrayList<>();
        synchronized (timerGuard) {
            while (!timers.isEmpty() && timers.peek().targetTime <= now) {
                Timer t = timers.poll();
                out.add(t.timerId);
                if (t.repeat) {
                    t.targetTime = now + t.interval;
                    timers.add(t);
                }
            }
        }
        return out;
    }

    public void clear() {
        synchronized (timerGuard) {
            timers.clear();
        }
    }

    /** For the checks: make the queue unreadable (another RN), leaving the method. */
    public void hideQueue() {
        timers = null;
    }

    // Kotlin `internal fun hasActiveTimersInRange(rangeMs: Long): Boolean`.
    public boolean hasActiveTimersInRange$ReactAndroid_release(long rangeMs) {
        lastRange = rangeMs;
        PriorityQueue<Timer> q = timers;
        if (q == null) q = hidden;
        synchronized (timerGuard) {
            for (Timer t : q) {
                if (!t.repeat && t.interval < rangeMs) return true;
            }
        }
        return false;
    }

    private final PriorityQueue<Timer> hidden = new PriorityQueue<>(11, (a, b) -> Long.signum(a.targetTime - b.targetTime));

    /** With the queue hidden, timers go here (the method still sees them). */
    public void createHidden(int timerId, long now, long delay, boolean repeat) {
        synchronized (timerGuard) {
            hidden.add(new Timer(timerId, now + delay, (int) delay, repeat));
        }
    }

    public void clearHidden() {
        synchronized (timerGuard) {
            hidden.clear();
        }
    }
}
