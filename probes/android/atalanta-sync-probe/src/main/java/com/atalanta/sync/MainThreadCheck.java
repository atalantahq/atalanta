// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

/**
 * Whether the main thread is idle, asked *on* the main thread the way
 * Espresso's {@code Interrogator} does: a sentinel posted at the front of the
 * main queue must run within {@link #BUDGET_MS}, and once it runs nothing
 * else may be due. {@code MessageQueue.isIdle()} read from another thread is
 * true while the main thread is executing a long message with nothing
 * queued behind it (AOSP {@code MessageQueue.java:154-158}), so jank read as idle.
 */
public final class MainThreadCheck {
    public static final long BUDGET_MS = 16;

    /** Posts at the front of the main queue; false when the looper is gone. */
    public interface Poster {
        boolean postAtFront(Runnable r);
    }

    /** Evaluated on the main thread: nothing else due now. */
    public interface QueueIdle {
        boolean isIdle();
    }

    private final MainThreadSampler sampler;

    public MainThreadCheck(Poster poster, QueueIdle queue) {
        this(poster, queue, BUDGET_MS);
    }

    public MainThreadCheck(Poster poster, QueueIdle queue, long budgetMs) {
        this.sampler = new MainThreadSampler(poster, () -> queue == null || queue.isIdle() ? 1 : 0, budgetMs, 0);
    }

    public boolean isIdle() {
        return sampler.read(0) == 1;
    }
}
