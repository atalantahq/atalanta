package com.atalanta.tree;

import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Bounds on the Compose testTag reads of a full dump (D-53, review MAJOR 5).
 *
 * <p>Each read is a synchronous refreshWithExtraData binder call the app's
 * main thread must serve (it can't come from the prefetched node cache), so
 * a screen with many tagged nodes could add 200 main-thread round trips to
 * every dump, each queued behind a frame while the app animates. Bounded:
 * <ul>
 *   <li>at most {@link #BUDGET_NS} (50 ms) of reads per dump, and at most
 *       {@link #MAX_READS} of them;</li>
 *   <li>a tag (or its absence) is cached per node identity (window id and
 *       the node's accessibility id) for {@link #TTL_NS}, so the next dumps of
 *       the same screen read nothing; at most {@link #MAX_CACHED} are kept.</li>
 * </ul>
 * A node left unread for lack of budget is read on a later dump, so the
 * tags fill in over a few polls on a large screen. Pure: the clock is
 * passed in (checked on the host JVM by HostChecks).
 */
final class TagReads {
    static final long BUDGET_NS = 50_000_000L;
    static final int MAX_READS = 200;
    static final int MAX_CACHED = 4096;
    /**
     * 2 s: a semantics node keeps its id across recomposition, so a tag
     * that changes (`testTag(if (selected) "tab_selected" else "tab")`)
     * reads stale until the entry expires; 10 s outlived most element
     * timeouts (review-fixes #5). 2 s is the driver's own lookup-cache TTL.
     */
    static final long TTL_NS = 2_000_000_000L;

    private static final class Tag {
        final String tag;
        final long at;
        Tag(String tag, long at) { this.tag = tag; this.at = at; }
    }

    private final LinkedHashMap<Long, Tag> cache = new LinkedHashMap<Long, Tag>(256, 0.75f, true) {
        @Override
        protected boolean removeEldestEntry(Map.Entry<Long, Tag> eldest) {
            return size() > MAX_CACHED;
        }
    };
    private long deadline;
    private int left;
    /** Binder reads made in the current dump (for tests and logs). */
    int reads;

    /** A new dump begins at `now`; `on` false reads (and serves) nothing. */
    void start(long now, boolean on) {
        deadline = now + BUDGET_NS;
        left = on ? MAX_READS : 0;
        reads = 0;
    }

    /** Whether the current dump reads tags at all. */
    boolean on() { return left > 0 || reads > 0; }

    /**
     * The cached tag of node `key`: "" for a node known to have none, null
     * when unknown or expired.
     */
    String cached(long key, long now) {
        Tag e = cache.get(key);
        if (e == null) return null;
        if (now - e.at > TTL_NS) {
            cache.remove(key);
            return null;
        }
        return e.tag;
    }

    /** Whether one more binder read fits this dump's budget. */
    boolean mayRead(long now) {
        return left > 0 && now < deadline;
    }

    /** Record a read of node `key` (`tag` null: it has none). */
    void read(long key, String tag, long now) {
        left--;
        reads++;
        cache.put(key, new Tag(tag == null ? "" : tag, now));
    }

    /** A node's identity across dumps. */
    static long key(int windowId, int nodeHash) {
        return ((long) windowId << 32) ^ (nodeHash & 0xffffffffL);
    }
}

// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only
