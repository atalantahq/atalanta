package com.atalanta.sync;

import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Delayed work on the main looper: messages posted with
 * {@code Handler.postDelayed} / {@code postAtTime} / {@code sendMessageDelayed}
 * that are due within the timer horizon (1.5 s, or the host connection's
 * {@code max_timer_ms}). The Android analogue of the iOS probe's
 * {@code TimerTracker}; before this the Android probe tracked no timers and
 * {@code max_timer_ms} had nothing to apply to.
 *
 * <p>How the queue is read:
 * <ol>
 * <li>Walking {@code MessageQueue.mMessages} / {@code Message.next} by
 *     reflection, under the queue's own lock (what {@code MessageQueue.dump}
 *     does).</li>
 * <li>Else {@code Looper.dump} (public API) parsed: one line per message,
 *     {@code { when=+1s500ms callback=... target=... }}.</li>
 * <li>Else "unknown" (one warning), never idle.</li>
 * </ol>
 * A walk that finds nothing is checked against the dump at most once a
 * second: a queue implementation that no longer keeps its messages in
 * {@code mMessages} (Android 16's concurrent queue may not) would otherwise
 * read as "no timers" forever. A message posted between the two reads is
 * not a disagreement (the walk is repeated); only {@link #VERIFY_DISAGREEMENTS}
 * empty re-walks in a row against a non-empty dump switch to the dump, and
 * then for {@link #DUMP_RETRY_MS} only, never for good (the dump formats
 * every message under the queue's lock). The walk copies the raw fields
 * under the lock and builds everything else after it.
 *
 * <p>Which messages count: due later than now (work due now is the
 * {@code main_looper} check's), due within the horizon, and first seen due
 * within it (a 10 s timeout that is 1 s from firing is not short work, as
 * on iOS, where a timer counts only when scheduled within the horizon).
 * Barriers and framework callbacks that repost themselves for as long as a
 * view is on screen ({@link #DEFAULT_IGNORED}, e.g. the cursor blink) don't
 * count. The ignore list is matched against the message's callback class,
 * and against its target (Handler) class only for a message without a
 * callback: {@code View.postDelayed} goes through the view root's handler
 * ({@code ViewRootImpl$ViewRootHandler}) and must count.
 *
 * <p>Pollers ({@link RepostChains}): a callback instance that is posted
 * again right as it fires, {@link RepostChains#REPOSTS} times in a row,
 * stops counting and is reported as {@code ignored: perpetual timer}. Keyed
 * by the callback instance (and, for a coroutine's {@code delay()} loop,
 * which posts a new lambda instance each time, by class plus a matching
 * period): one poller never hides unrelated work of the same class, and a
 * debounce (removed before it fired, or re-armed by the next tap after it
 * fired) always counts.
 */
public final class LooperTimers implements Tracker {
    /** The horizon when the connection set none (EarlGrey's and Detox's default). */
    public static final long DEFAULT_MAX_TIMER_MS = 1_500;
    /**
     * Callback classes (prefixes) that never count; for a message without a
     * callback, its target class. Framework re-posters only.
     */
    public static final List<String> DEFAULT_IGNORED = Arrays.asList(
            "android.widget.Editor$Blink",
            "android.view.Choreographer",
            "android.widget.TextView$Marquee");
    /** Prefixes added process-wide ({@code AtalantaSyncProbe.ignoreTimerCallbacks}), before or after start. */
    private static final List<String> GLOBAL_IGNORED = new CopyOnWriteArrayList<>();
    /** How often an empty walk is checked against the dump. */
    static final long VERIFY_MS = 1_000;
    /** Empty walks in a row, each re-walked, that the dump must contradict before it is used. */
    static final int VERIFY_DISAGREEMENTS = 2;
    /** How long the dump is used before the walk is tried again. */
    static final long DUMP_RETRY_MS = 10_000;
    /** The longest queue walked (a corrupt list must not spin). */
    static final int MAX_MESSAGES = 10_000;

    /** {@code SystemClock.uptimeMillis} (what {@code Message.when} is in). */
    public interface Clock {
        long uptimeMs();
    }

    /** One line of a dump. */
    public interface Line {
        void line(String s);
    }

    /** {@code Looper.dump} into {@code out}. */
    public interface Dumper {
        void dump(Line out);
    }

    /** One message, as read. */
    static final class Msg {
        /** callback class | target class | what (what only without a callback). */
        final String key;
        /** The callback instance (or target instance + what): what a re-post is keyed by. */
        final String identity;
        final boolean hasCallback;
        final long when;
        final boolean barrier;

        Msg(String key, String identity, boolean hasCallback, long when, boolean barrier) {
            this.key = key;
            this.identity = identity;
            this.hasCallback = hasCallback;
            this.when = when;
            this.barrier = barrier;
        }
    }

    private static final Pattern DUMP_LINE = Pattern.compile("Message \\d+: \\{ when=(\\S+)(.*)\\}");
    private static final Pattern DUMP_PART = Pattern.compile("(\\d+)(ms|d|h|m|s)");

    private final Object queue;
    private final Dumper dumper;
    private final Clock clock;
    private final List<String> ignored = new CopyOnWriteArrayList<>(DEFAULT_IGNORED);
    private final RepostChains chains = RepostChains.sameSample();

    /** The walk can't work here at all (no queue, no fields): dump only. */
    private volatile boolean walkBroken;
    /** The dump is in use until this time (the walk disagreed with it). */
    private volatile long dumpUntil = Long.MIN_VALUE;
    private int disagreements;
    private Field messagesField;
    private Field nextField;
    private Field whenField;
    private Field targetField;
    private Field callbackField;
    private Field whatField;
    private long lastVerify = Long.MIN_VALUE;
    private volatile boolean perpetualSeen;

    /**
     * {@code queue}: the main looper's {@code MessageQueue} (null: dump
     * only); {@code dumper}: {@code Looper.dump} (null: walk only).
     */
    public LooperTimers(Object queue, Dumper dumper, Clock clock) {
        this.queue = queue;
        this.dumper = dumper;
        this.clock = clock;
        if (queue == null) walkBroken = true;
    }

    @Override
    public String name() {
        return "timer";
    }

    /** Callback class prefixes that never count (added to the defaults), this instance. */
    public void ignoreCallbacks(List<String> prefixes) {
        for (String p : prefixes) {
            if (p != null && !p.isEmpty() && !ignored.contains(p)) ignored.add(p);
        }
    }

    /** Callback class prefixes that never count, for every instance (set before or after start). */
    public static void ignoreCallbacksGlobally(List<String> prefixes) {
        for (String p : prefixes) {
            if (p != null && !p.isEmpty() && !GLOBAL_IGNORED.contains(p)) GLOBAL_IGNORED.add(p);
        }
    }

    /** For the host checks. */
    static void clearGlobalIgnores() {
        GLOBAL_IGNORED.clear();
    }

    /** Whether the reflective walk is in use (false: the dump, or nothing). */
    public boolean walking() {
        return !walkBroken && clock.uptimeMs() >= dumpUntil;
    }

    @Override
    public List<String> ignoredPerpetual() {
        return perpetualSeen ? Collections.singletonList("timer") : Collections.<String>emptyList();
    }

    @Override
    public int pending(long maxTimerMs) {
        long horizon = maxTimerMs < 0 ? DEFAULT_MAX_TIMER_MS : maxTimerMs;
        long now = clock.uptimeMs();
        List<Msg> msgs = read(now);
        if (msgs == null) {
            ProbeLog.warnOnce("timer-unknown",
                    "can't read the main looper's queue (no MessageQueue walk, no Looper.dump): timers not waited for");
            return UNKNOWN;
        }
        return count(msgs, now, horizon);
    }

    synchronized int count(List<Msg> msgs, long now, long horizon) {
        List<RepostChains.Item> items = new ArrayList<>();
        List<Msg> real = new ArrayList<>();
        for (Msg m : msgs) {
            if (m.barrier) continue;
            real.add(m);
            items.add(new RepostChains.Item(m.identity, m.key, m.when, -1));
        }
        List<RepostChains.Verdict> verdicts = chains.sample(items, now);
        int pending = 0;
        boolean perpetual = false;
        for (int i = 0; i < real.size(); i++) {
            Msg m = real.get(i);
            RepostChains.Verdict v = verdicts.get(i);
            long due = m.when - now;
            if (due <= 0 || due > horizon || v.firstDue > horizon) continue;
            if (isIgnored(m)) continue;
            if (v.perpetual) {
                perpetual = true;
                ProbeLog.warnOnce("timer-perpetual:" + m.key,
                        "main-looper callback " + m.key + " re-posts itself as it fires (a poller): not waited for");
                continue;
            }
            pending++;
        }
        perpetualSeen = perpetual;
        return pending;
    }

    private boolean isIgnored(Msg m) {
        // key: callback|target|what. The target only matters without a callback.
        int bar = m.key.indexOf('|');
        int bar2 = m.key.indexOf('|', bar + 1);
        String callback = m.key.substring(0, bar);
        String target = m.key.substring(bar + 1, bar2 < 0 ? m.key.length() : bar2);
        String cls = m.hasCallback || !callback.isEmpty() ? callback : target;
        for (String p : ignored) {
            if (cls.startsWith(p)) return true;
        }
        for (String p : GLOBAL_IGNORED) {
            if (cls.startsWith(p)) return true;
        }
        return false;
    }

    /** The queue's messages, or null when neither way can read them. */
    private List<Msg> read(long now) {
        if (!walkBroken && now >= dumpUntil) {
            List<Msg> walked = walk();
            if (walked == null) {
                walkBroken = true;
                ProbeLog.warnOnce("timer-walk", "MessageQueue walk unavailable; reading timers from Looper.dump");
            } else if (!walked.isEmpty() || dumper == null) {
                synchronized (this) {
                    disagreements = 0;
                }
                return walked;
            } else {
                // Empty: trust it, but check against the dump now and then.
                synchronized (this) {
                    if (lastVerify != Long.MIN_VALUE && now - lastVerify < VERIFY_MS) return walked;
                    lastVerify = now;
                }
                List<Msg> dumped = dump(now);
                if (dumped == null || dumped.isEmpty()) {
                    synchronized (this) {
                        disagreements = 0;
                    }
                    return walked;
                }
                // A message posted between the two reads is not a disagreement:
                // walk again; only a walk that is still empty disagrees.
                List<Msg> again = walk();
                if (again == null || !again.isEmpty()) {
                    synchronized (this) {
                        disagreements = 0;
                    }
                    return again == null ? dumped : again;
                }
                synchronized (this) {
                    if (++disagreements < VERIFY_DISAGREEMENTS) {
                        lastVerify = Long.MIN_VALUE; // verify again on the next sample
                        return dumped;
                    }
                    disagreements = 0;
                    dumpUntil = now + DUMP_RETRY_MS;
                }
                ProbeLog.warnOnce("timer-walk-empty",
                        "MessageQueue walk sees no messages but Looper.dump does; reading timers from Looper.dump");
                return dumped;
            }
        }
        return dumper == null ? null : dump(now);
    }

    /** Walk {@code mMessages}; null when this queue can't be walked. */
    private List<Msg> walk() {
        try {
            if (messagesField == null) {
                messagesField = Reflect.field(queue.getClass(), "mMessages");
                if (messagesField == null) return null;
            }
            // Copy the raw fields under the queue's lock; build keys after it.
            List<Object[]> raw = new ArrayList<>();
            synchronized (queue) {
                Object m = messagesField.get(queue);
                int n = 0;
                while (m != null) {
                    if (++n > MAX_MESSAGES) return null;
                    if (nextField == null && !messageFields(m.getClass())) return null;
                    raw.add(new Object[] {whenField.getLong(m), targetField.get(m),
                            callbackField == null ? null : callbackField.get(m),
                            whatField == null ? null : whatField.get(m)});
                    m = nextField.get(m);
                }
            }
            List<Msg> out = new ArrayList<>(raw.size());
            for (Object[] r : raw) out.add(message((Long) r[0], r[1], r[2], r[3]));
            return out;
        } catch (Throwable t) {
            return null;
        }
    }

    private boolean messageFields(Class<?> c) {
        Field when = Reflect.field(c, "when");
        Field target = Reflect.field(c, "target");
        Field next = Reflect.field(c, "next");
        if (when == null || target == null || next == null) return false;
        whenField = when;
        targetField = target;
        callbackField = Reflect.field(c, "callback");
        whatField = Reflect.field(c, "what");
        nextField = next;
        return true;
    }

    private static Msg message(long when, Object t, Object cb, Object w) {
        String key = (cb == null ? "" : cb.getClass().getName()) + "|"
                + (t == null ? "" : t.getClass().getName()) + "|"
                + (cb == null && w != null ? w : "");
        String identity = cb != null
                ? cb.getClass().getName() + "#" + Integer.toHexString(System.identityHashCode(cb))
                : key + "#" + (t == null ? "0" : Integer.toHexString(System.identityHashCode(t)));
        return new Msg(key, identity, cb != null, when, t == null);
    }

    /** Parse {@code Looper.dump}; null when it can't be read. */
    private List<Msg> dump(long now) {
        final List<String> lines = new ArrayList<>();
        try {
            dumper.dump(lines::add);
        } catch (Throwable t) {
            return null;
        }
        return parseDump(lines, now);
    }

    /** {@code Message.toString(now)} lines as messages; null when none of them parse. */
    static List<Msg> parseDump(List<String> lines, long now) {
        List<Msg> out = new ArrayList<>();
        boolean sawHeader = false;
        for (String line : lines) {
            if (line == null) continue;
            if (line.contains("Looper (") || line.contains("Total messages")) sawHeader = true;
            Matcher m = DUMP_LINE.matcher(line);
            if (!m.find()) continue;
            long delta = parseDuration(m.group(1));
            if (delta == Long.MIN_VALUE) continue;
            String rest = m.group(2);
            if (rest.contains("barrier=")) {
                out.add(new Msg("||", "barrier@" + (now + delta), false, now + delta, true));
                continue;
            }
            String callback = attr(rest, "callback=");
            String target = attr(rest, "target=");
            String what = callback.isEmpty() ? attr(rest, "what=") : "";
            String key = callback + "|" + target + "|" + what;
            // No instances in a dump: each message is its own identity, so
            // re-posts are linked by class and period only.
            out.add(new Msg(key, key + "@" + (now + delta), !callback.isEmpty(), now + delta, false));
        }
        return out.isEmpty() && !sawHeader ? null : out;
    }

    private static String attr(String s, String name) {
        int i = s.indexOf(name);
        if (i < 0) return "";
        int start = i + name.length();
        int end = s.indexOf(' ', start);
        return s.substring(start, end < 0 ? s.length() : end).trim();
    }

    /** {@code TimeUtils.formatDuration}: "+1s500ms", "-12ms", "0"; MIN_VALUE when not one. */
    static long parseDuration(String s) {
        if (s.equals("0")) return 0;
        if (s.isEmpty() || (s.charAt(0) != '+' && s.charAt(0) != '-')) return Long.MIN_VALUE;
        long sign = s.charAt(0) == '-' ? -1 : 1;
        Matcher m = DUMP_PART.matcher(s);
        long total = 0;
        int pos = 1;
        while (m.find()) {
            if (m.start() != pos) return Long.MIN_VALUE;
            long v = Long.parseLong(m.group(1));
            switch (m.group(2)) {
                case "d": total += v * 86_400_000L; break;
                case "h": total += v * 3_600_000L; break;
                case "m": total += v * 60_000L; break;
                case "s": total += v * 1_000L; break;
                default: total += v; break;
            }
            pos = m.end();
        }
        return pos == s.length() && pos > 1 ? sign * total : Long.MIN_VALUE;
    }
}
