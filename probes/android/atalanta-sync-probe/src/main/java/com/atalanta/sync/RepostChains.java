// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.Iterator;
import java.util.List;
import java.util.Map;

/**
 * Tells pollers from one-shot work across samples of a queue of delayed
 * items (main-looper messages, React Native's JS timers).
 *
 * <p>A poller is an item that, when it fires, is immediately posted again
 * (from inside its own callback). Each such re-post extends a chain; an item
 * at the end of a chain of {@link #REPOSTS} or more re-posts in a row is
 * perpetual and doesn't count. Everything else starts a new chain at 0 and
 * counts:
 * <ul>
 * <li>an item removed before it fired and posted again (a debounce) is
 *     not a re-post (it never fired);</li>
 * <li>an item posted again some time after it fired (a debounce re-armed by
 *     the next tap) is not either. Main-looper items are linked only when the
 *     successor is in the very sample that saw the predecessor gone (a
 *     poller re-posts from inside its own callback, so there is no sample in
 *     between that saw neither); JS timers, whose re-post takes a round trip
 *     through the JS thread, within {@link #LINK_WINDOW_MS};</li>
 * <li>an unrelated item of the same class (another coroutine's
 *     {@code delay()}, another RxAndroid task) is linked only when its delay
 *     matches the fired one's period, and each fired item links at most one
 *     successor (the same instance first).</li>
 * </ul>
 *
 * Links are made by the item's identity first (a {@code Runnable} posting
 * itself again; the identity, not the class: many unrelated posts share one
 * lambda class), else by class with a matching period. When the samples
 * had a gap (nobody was asking between two steps) nothing is linked by
 * class and a chain is only carried over (not extended) to the same
 * identity.
 *
 * <p>Pollers that post a new item each time (a JS {@code setTimeout} poll,
 * a coroutine's {@code delay()} loop) fire unseen between steps, so once a
 * chain is perpetual its <em>poster</em> (the identity, or the class plus
 * period) is remembered: while it has no live item, the next fresh item that
 * matches it (same identity, or same class with the period within tolerance)
 * and is <em>in phase</em> with it (due a whole number of periods after the
 * poster's last item, see {@link #inPhase}) is its re-post and stays
 * perpetual, whether the samples had a gap or not. Of several fresh items,
 * the one closest in phase is taken. A poster is forgotten at once when its
 * item is removed before it was due ({@code clearTimeout},
 * {@code removeCallbacks}, a cancelled coroutine: the poll stopped), once
 * continuous sampling sees it without a live item for longer than
 * {@link #LINK_WINDOW_MS} (it stopped firing), or after a quiet gap of
 * {@link #POSTER_QUIET_MS} (4 periods at least) without any of its items
 * seen. A one-shot of the same class and period next to a live poller is
 * never taken for it (one live item per poster), nor is one the step's action
 * created when the poll was cancelled or fired in the same gap: it is almost
 * never in phase. A one-shot is never promoted on its own: only a chain that
 * became perpetual under continuous sampling leaves a poster.
 */
final class RepostChains {
    /** Re-posts of the same instance in a row (fired, then immediately posted again) that make a poller. */
    static final int REPOSTS = 2;
    /**
     * Re-posts linked by class only (a new instance each time: a coroutine's
     * {@code delay()} loop, a JS {@code setTimeout} poll) that make a poller,
     * and for at least {@link #CLASS_SPAN_MS}: a few sequential
     * {@code delay()}s of one coroutine are not a poller.
     */
    static final int CLASS_REPOSTS = 3;
    static final long CLASS_SPAN_MS = 2_000;
    /** A re-post this long after the last one is a new one-shot, not a poller. */
    static final long MAX_PERIOD_MS = 3_000;
    /** How soon after its predecessor fired a re-post must appear. */
    static final long LINK_WINDOW_MS = 200;
    /** Samples further apart than this are not continuous (nothing to link on). */
    static final long SAMPLE_GAP_MS = 250;
    /** A perpetual poster not seen for this long (and 4 periods) is forgotten. */
    static final long POSTER_QUIET_MS = 5_000;
    /**
     * An item gone this much before its due time was removed, not fired (the
     * sample's clock is read just before the queue, so a firing can look a
     * little early).
     */
    static final long REMOVED_SLACK_MS = 5;

    /** For main-looper items: a re-post must be in the sample that saw its predecessor gone. */
    static RepostChains sameSample() {
        return new RepostChains(0);
    }

    /** How long after its predecessor vanished a re-post may first be seen. */
    private final long linkWindowMs;

    /** JS timers: a re-post within {@link #LINK_WINDOW_MS}. */
    RepostChains() {
        this(LINK_WINDOW_MS);
    }

    private RepostChains(long linkWindowMs) {
        this.linkWindowMs = linkWindowMs;
    }

    /** A perpetual chain's poster, kept across firings and sampling gaps. */
    private static final class Poster {
        final String identity;
        final String cls;
        final boolean byClass;
        final long period;
        /** Its live item, or null between a firing and the re-post. */
        Track current;
        /** When an item of it was last seen live. */
        long lastSeen;
        /** When {@link #current} went (fired or removed). */
        long goneAt;
        /** The due time of its last item: its re-posts are due whole periods after it. */
        long lastWhen;

        Poster(Track t, long now) {
            identity = t.identity;
            cls = t.cls;
            byClass = t.byClass;
            period = t.period;
            current = t;
            lastSeen = now;
            lastWhen = t.when;
        }

        long expiry() {
            return Math.max(POSTER_QUIET_MS, 4 * Math.max(0, period));
        }

        /** How far {@code it} is out of phase with this poster (smaller is closer), or -1: not its re-post. */
        long phaseError(Item it) {
            // The same instance posted again is its re-post (its cancellation already dropped it).
            if (!byClass) return identity.equals(it.identity) ? 0 : -1;
            if (!cls.equals(it.cls) || period <= 0) return -1;
            if (it.period >= 0 && Math.abs(it.period - period) > Math.max(50, period / 4)) return -1;
            return inPhase(lastWhen, it.when, period);
        }
    }

    /**
     * Whether an item due at {@code when} is due a whole number k >= 1 of
     * periods after {@code last}: a poller's re-post is (each cycle is the
     * period plus a little latency: the firing's lateness, the JS round trip,
     * work before the re-post), a one-shot the step's action created almost
     * never is. The window for k cycles is
     * {@code [k*period - early, k*(period + late) + early]}; the windows stay
     * apart for the {@link #POSTER_QUIET_MS} a poster is kept. Returns the
     * distance from the window's start, or -1 when out of phase.
     */
    static long inPhase(long last, long when, long period) {
        long d = when - last;
        if (d <= 0) return -1;
        long early = Math.max(20, period / 20);
        long late = Math.max(30, period / 10);
        long k = Math.max(1, (d + early) / period);
        for (long j = Math.max(1, k - 1); j <= k + 1; j++) {
            long lo = j * period - early;
            long hi = j * (period + late) + early;
            if (d >= lo && d <= hi) return Math.abs(d - j * period);
        }
        return -1;
    }

    /** One item, as read. {@code period}: its delay when known (JS timers), else -1. */
    static final class Item {
        final String identity;
        final String cls;
        final long when;
        final long period;

        Item(String identity, String cls, long when, long period) {
            this.identity = identity;
            this.cls = cls;
            this.when = when;
            this.period = period;
        }

        String id() {
            return identity + "@" + when;
        }
    }

    private static final class Track {
        String identity;
        String cls;
        long when;
        long firstDue;
        long period = -1;
        int chain;
        boolean byClass;
        long chainStart;
        long vanishedAt;
        /** Vanished during continuous sampling (can be linked as a re-post). */
        boolean certain;
        /** Set once its chain is perpetual. */
        Poster poster;
    }

    /** Per item of the last sample. */
    static final class Verdict {
        /** How far out it was when first seen. */
        final long firstDue;
        final boolean perpetual;

        Verdict(long firstDue, boolean perpetual) {
            this.firstDue = firstDue;
            this.perpetual = perpetual;
        }
    }

    private Map<String, Track> live = new HashMap<>();
    private final List<Track> fired = new ArrayList<>();
    private final List<Poster> posters = new ArrayList<>();
    private long lastSample = Long.MIN_VALUE;

    /** Verdicts for {@code items} (same order), sampled at {@code now}. */
    synchronized List<Verdict> sample(List<Item> items, long now) {
        boolean continuous = lastSample != Long.MIN_VALUE && now - lastSample <= SAMPLE_GAP_MS;
        if (!continuous) {
            for (Track t : fired) t.certain = false;
        }
        Map<String, Track> next = new HashMap<>();
        List<Item> fresh = new ArrayList<>();
        for (Item it : items) {
            String id = it.id();
            Track t = live.remove(id);
            if (t != null) {
                next.put(id, t);
            } else if (!next.containsKey(id)) {
                fresh.add(it);
            }
        }
        // The rest of the last sample is gone: fired (due), or removed.
        for (Track t : live.values()) {
            if (t.when <= now) {
                t.vanishedAt = now;
                t.certain = continuous;
                fired.add(t);
            }
        }
        for (Iterator<Track> i = fired.iterator(); i.hasNext(); ) {
            Track t = i.next();
            long age = now - t.vanishedAt;
            if (age > MAX_PERIOD_MS || (t.certain && age > linkWindowMs)) i.remove();
        }
        // Posters whose item went this sample; forget those that stopped.
        for (Iterator<Poster> i = posters.iterator(); i.hasNext(); ) {
            Poster p = i.next();
            if (p.current != null && next.get(id(p.current)) != p.current) {
                if (p.current.when > now + REMOVED_SLACK_MS) {
                    // Removed before it was due: the poll was cancelled, it stopped.
                    p.current.poster = null;
                    i.remove();
                    continue;
                }
                p.current = null;
                p.goneAt = now;
            }
            if (p.current != null) {
                p.lastSeen = now;
            } else if (now - p.lastSeen > p.expiry() || (continuous && now - p.goneAt > LINK_WINDOW_MS)) {
                i.remove();
            }
        }
        Map<Item, Poster> adopted = adoptions(fresh, now);
        for (Item it : fresh) {
            Track t = new Track();
            t.identity = it.identity;
            t.cls = it.cls;
            t.when = it.when;
            t.firstDue = it.when - now;
            t.period = it.period >= 0 ? it.period : (continuous ? Math.max(0, it.when - now) : -1);
            t.chainStart = now;
            Track p = predecessor(it, now, continuous, t.period);
            if (p != null) {
                fired.remove(p);
                boolean repost = continuous && p.certain;
                t.chain = repost ? p.chain + 1 : p.chain;
                t.byClass = p.byClass || !p.identity.equals(it.identity);
                t.chainStart = p.chainStart;
                if (t.period < 0) t.period = p.period;
                if (p.poster != null && p.poster.current == null) t.poster = p.poster;
            }
            if (t.poster == null) {
                Poster ps = adopted.get(it);
                if (ps != null && ps.current == null) t.poster = ps;
            }
            if (t.poster != null) {
                Poster ps = t.poster;
                ps.current = t;
                ps.lastSeen = now;
                ps.lastWhen = t.when;
                if (t.period < 0) t.period = ps.period;
                for (Iterator<Track> i = fired.iterator(); i.hasNext(); ) {
                    if (i.next().poster == ps) i.remove();
                }
            }
            next.put(it.id(), t);
        }
        for (Track t : next.values()) {
            if (t.poster == null && chainPerpetual(t, now)) {
                t.poster = new Poster(t, now);
                posters.add(t.poster);
            }
        }
        live = next;
        lastSample = now;
        List<Verdict> out = new ArrayList<>(items.size());
        for (Item it : items) {
            Track t = next.get(it.id());
            out.add(new Verdict(t.firstDue, perpetual(t, now)));
        }
        return out;
    }

    /** Whether any live item is perpetual (for "ignored: perpetual ..."). */
    synchronized boolean anyPerpetual() {
        for (Track t : live.values()) if (perpetual(t, lastSample)) return true;
        return false;
    }

    private static String id(Track t) {
        return t.identity + "@" + t.when;
    }

    /**
     * For each remembered poster without a live item, the fresh item that is
     * its re-post: the one closest in phase (not the first in queue order: a
     * one-shot due before the poll's re-post must not take its place).
     */
    private Map<Item, Poster> adoptions(List<Item> fresh, long now) {
        Map<Item, Poster> out = new java.util.IdentityHashMap<>();
        for (Poster p : posters) {
            if (p.current != null || now - p.lastSeen > p.expiry()) continue;
            Item best = null;
            long bestErr = Long.MAX_VALUE;
            for (Item it : fresh) {
                if (out.containsKey(it)) continue;
                long err = p.phaseError(it);
                if (err >= 0 && err < bestErr) {
                    best = it;
                    bestErr = err;
                }
            }
            if (best != null) out.put(best, p);
        }
        return out;
    }

    private static boolean perpetual(Track t, long now) {
        return t.poster != null || chainPerpetual(t, now);
    }

    private static boolean chainPerpetual(Track t, long now) {
        if (!t.byClass) return t.chain >= REPOSTS;
        return t.chain >= CLASS_REPOSTS && now - t.chainStart >= CLASS_SPAN_MS;
    }

    private Track predecessor(Item it, long now, boolean continuous, long period) {
        // The same instance, re-posted after it fired.
        for (Track p : fired) {
            if (p.identity.equals(it.identity) && it.when > p.when && it.when - p.when <= MAX_PERIOD_MS) {
                // Across a sampling gap: carry its chain over, never start one.
                if (continuous && p.certain) return p;
                return p.chain > 0 ? p : null;
            }
        }
        if (!continuous || period < 0) return null;
        // Another instance of the same class with the same period (a coroutine's delay loop).
        Track best = null;
        long bestDiff = Long.MAX_VALUE;
        for (Track p : fired) {
            if (!p.certain || !p.cls.equals(it.cls) || p.period < 0) continue;
            if (it.when <= p.when || it.when - p.when > MAX_PERIOD_MS) continue;
            long diff = Math.abs(period - p.period);
            if (diff > Math.max(50, p.period / 4)) continue;
            if (diff < bestDiff) {
                best = p;
                bestDiff = diff;
            }
        }
        return best;
    }
}
