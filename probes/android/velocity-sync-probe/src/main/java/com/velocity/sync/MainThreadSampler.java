package com.velocity.sync;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Reads a value <em>on</em> the main thread and waits a bounded time for it.
 * Some framework state is per-thread: {@code ValueAnimator}'s running count
 * lives in a {@code ThreadLocal<AnimationHandler>}, so read from the probe's
 * socket thread it is always 0.
 *
 * <p>At most one sentinel is outstanding: while an earlier one hasn't run,
 * the next read waits on it instead of posting another. Timed-out sentinels
 * used to pile up at the front of a blocked main queue, and the first to run
 * then saw the rest due and read busy.
 */
public final class MainThreadSampler {
    /** Evaluated on the main thread. */
    public interface Sample {
        int read();
    }

    private static final class Pending {
        final CountDownLatch ran = new CountDownLatch(1);
        volatile int value;
    }

    private final MainThreadCheck.Poster poster;
    private final Sample sample;
    private final long budgetMs;
    private final int onError;
    private Pending pending; // guarded by this

    /** {@code onError}: the value when the sample throws on the main thread. */
    public MainThreadSampler(MainThreadCheck.Poster poster, Sample sample, long budgetMs, int onError) {
        this.poster = poster;
        this.sample = sample;
        this.budgetMs = budgetMs;
        this.onError = onError;
    }

    /**
     * The sample's value, or {@code onTimeout} when the main thread didn't
     * run it within the budget or refuses posts. A poster may run the
     * sentinel inline when it is already on the main thread.
     */
    public int read(int onTimeout) {
        Pending p;
        synchronized (this) {
            p = pending;
            if (p == null || p.ran.getCount() == 0) {
                final Pending next = new Pending();
                pending = next;
                boolean posted = poster.postAtFront(() -> {
                    int v;
                    try {
                        v = sample.read();
                    } catch (Throwable t) {
                        // never throw on the app's main thread
                        v = onError;
                    }
                    next.value = v;
                    next.ran.countDown();
                });
                if (!posted) {
                    pending = null;
                    return onTimeout;
                }
                p = next;
            }
        }
        try {
            if (!p.ran.await(budgetMs, TimeUnit.MILLISECONDS)) return onTimeout;
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return onTimeout;
        }
        return p.value;
    }
}
