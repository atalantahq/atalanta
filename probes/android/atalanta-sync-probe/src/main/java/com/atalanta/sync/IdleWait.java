// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

/**
 * Answers "idle" only once idle has held: on at least {@link #MIN_SAMPLES}
 * consecutive samples {@link #POLL_MS} apart, and for the host's
 * {@code quiet_ms} when it sends one. The probe used to answer on the first
 * 5 ms sample, which a gap between two bursts of main-thread work satisfies.
 * Off-main-thread analogue of EarlGrey's "idle three times in a row".
 */
public final class IdleWait {
    public static final long POLL_MS = 16;
    public static final int MIN_SAMPLES = 3;
    /** The longest hold a host may ask for. */
    public static final long MAX_QUIET_MS = 1_000;
    /** The longest wait a host may ask for (a client could park a thread). */
    public static final long MAX_TIMEOUT_MS = 60_000;

    /** One idle sample. */
    public interface Check {
        boolean isIdle();
    }

    public static final class Outcome {
        public final boolean idle;
        public final long waitedMs;

        Outcome(boolean idle, long waitedMs) {
            this.idle = idle;
            this.waitedMs = waitedMs;
        }
    }

    private IdleWait() {}

    /** {@code quietMs} < 0 means "not sent". */
    public static Outcome await(long timeoutMs, long quietMs, Check check) {
        return await(timeoutMs, quietMs, POLL_MS, check);
    }

    public static Outcome await(long timeoutMs, long quietMs, long pollMs, Check check) {
        long timeout = Math.min(Math.max(timeoutMs, 0), MAX_TIMEOUT_MS);
        long hold = Math.max(Math.min(quietMs, MAX_QUIET_MS), pollMs * (MIN_SAMPLES - 1));
        long start = System.nanoTime();
        long idleSince = -1;
        int samples = 0;
        while (true) {
            long now = (System.nanoTime() - start) / 1_000_000;
            if (check.isIdle()) {
                if (idleSince < 0) idleSince = now;
                samples++;
                if (samples >= MIN_SAMPLES && now - idleSince >= hold) {
                    return new Outcome(true, now);
                }
            } else {
                idleSince = -1;
                samples = 0;
            }
            if (now >= timeout) {
                return new Outcome(false, now);
            }
            try {
                Thread.sleep(Math.max(1, pollMs));
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return new Outcome(false, now);
            }
        }
    }
}
