// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.util.concurrent.atomic.AtomicInteger;

/**
 * In-flight requests the app reports (see {@code AtalantaSyncProbe.networkStarted}).
 * Nothing fed the old counter, so Android network sync was dead code.
 */
public final class NetworkTracker {
    public static final NetworkTracker SHARED = new NetworkTracker(UrlBlocklist.SHARED);

    /**
     * A response body the app hasn't finished (read to the end or closed)
     * and hasn't read from for this long (since its headers arrived, or its
     * last read that got data) is a leak (or a stalled stream): it stops
     * counting, with a warning. A slow download that is still being read keeps
     * counting however long it takes. {@code rn_network} has the same kind of
     * bound.
     */
    public static final long BODY_MAX_MS = 30_000;

    private final AtomicInteger inFlight = new AtomicInteger();
    private final UrlBlocklist blocklist;
    private final LooperTimers.Clock clock;
    /** Handles whose body is being read (or leaked), with their deadline. */
    private final java.util.Set<Handle> bodies = java.util.concurrent.ConcurrentHashMap.newKeySet();

    public NetworkTracker(UrlBlocklist blocklist) {
        this(blocklist, ReactNativeIdling.SYSTEM_CLOCK);
    }

    NetworkTracker(UrlBlocklist blocklist, LooperTimers.Clock clock) {
        this.blocklist = blocklist;
        this.clock = clock;
    }

    /**
     * A request to {@code url} started. Returns whether it counts: pass the
     * result to {@link #finished(boolean)}. Blocklisted URLs don't count.
     */
    public boolean started(String url) {
        if (blocklist.isIgnored(url)) return false;
        inFlight.incrementAndGet();
        return true;
    }

    /** A request ended; {@code tracked} is what {@link #started} returned. */
    public void finished(boolean tracked) {
        if (!tracked) return;
        decrement();
    }

    /** Counted unconditionally (the old {@code AppStateTracker.incrementNetwork}). */
    public void increment() {
        inFlight.incrementAndGet();
    }

    /** Never below zero: an unmatched decrement must not hide a later request. */
    public void decrement() {
        while (true) {
            int cur = inFlight.get();
            if (cur <= 0 || inFlight.compareAndSet(cur, cur - 1)) return;
        }
    }

    public int count() {
        if (!bodies.isEmpty()) {
            long now = clock.uptimeMs();
            for (Handle h : bodies) {
                if (now < h.bodyDeadline) continue;
                if (h.close()) {
                    ProbeLog.warnOnce("network-body-leak", "a response body was neither read to the end nor closed,"
                            + " nor read from for " + (BODY_MAX_MS / 1000) + " s (a leak, or a stream): no longer"
                            + " waited for; close it, or add its URL to ignore_urls");
                }
            }
        }
        return inFlight.get();
    }

    /**
     * A request that ends when {@link Handle#close()} is called (its
     * response body closed or read to the end), exactly once however often
     * it is called. Null for a blocklisted URL (nothing counted).
     */
    public Handle open(String url) {
        return started(url) ? new Handle(this) : null;
    }

    /** One counted request; {@link #close()} is idempotent and thread-safe. */
    public static final class Handle {
        private final NetworkTracker tracker;
        private final java.util.concurrent.atomic.AtomicBoolean closed = new java.util.concurrent.atomic.AtomicBoolean();

        Handle(NetworkTracker tracker) {
            this.tracker = tracker;
        }

        private volatile long bodyDeadline = Long.MAX_VALUE;

        /** Closes it; whether this call did (false: it already was). */
        public boolean close() {
            if (!closed.compareAndSet(false, true)) return false;
            tracker.bodies.remove(this);
            tracker.decrement();
            return true;
        }

        /** The headers arrived; the body is read now, for at most {@link #BODY_MAX_MS}. */
        void bodyStarted() {
            bodyDeadline = tracker.clock.uptimeMs() + BODY_MAX_MS;
            tracker.bodies.add(this);
            if (closed.get()) tracker.bodies.remove(this);
        }

        /** A read got data: the body is still being read, its bound starts again. */
        void bodyProgress() {
            if (bodyDeadline != Long.MAX_VALUE) bodyDeadline = tracker.clock.uptimeMs() + BODY_MAX_MS;
        }

        public boolean closed() {
            return closed.get();
        }
    }
}
