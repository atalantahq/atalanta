package com.atalanta.sync;

import static com.atalanta.sync.HostChecks.check;

import android.os.Message;
import android.os.MessageQueue;
import java.util.ArrayList;
import java.util.List;

/**
 * Pollers across steps (review-fi4 MAJOR-1, m1): wait_idle samples every
 * 16 ms until idle, then nobody asks while the host acts, so a poller that
 * posts a new item each time fires unseen between steps.
 */
final class PollerChecks {
    private PollerChecks() {}

    static void run() {
        jsPollAcrossSteps();
        jsPollStopsThenOneShot();
        jsOneShotNextToPoll();
        coroutineLoopAcrossSteps();
        looperDebounceRearmedQuickly();
        jsPollClearedThenOneShot();
        jsPollStoppedInGapThenOneShot();
        jsOneShotDueBeforeRepost();
        coroutinePollCancelledThenOneShot();
    }

    /** A JS app with a {@code setTimeout(poll, period)} loop (re-armed 5 ms after it fires, a JS round trip). */
    static final class JsPoll {
        final TrackerChecks.Clock clock = new TrackerChecks.Clock();
        final TrackerChecks.Rn r = new TrackerChecks.Rn(true);
        final TrackerChecks.OldArchApp app = new TrackerChecks.OldArchApp();
        final ReactNativeIdling rn;
        final long period;
        boolean polling = true;
        int nextId = 100;
        /** Ids of the poll's timers (the rest are one-shots). */
        final List<Integer> pollIds = new ArrayList<>();
        long rearmAt = Long.MIN_VALUE;

        JsPoll(long period) {
            this.period = period;
            app.host.getReactInstanceManager().context = r.ctx;
            rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
            arm();
        }

        void arm() {
            pollIds.add(nextId);
            r.timers.createTimer(nextId++, clock.uptimeMs(), period, false);
        }

        /** 1 ms of app time: fire due timers, re-arm the poll after the JS round trip. */
        void tick() {
            clock.now.addAndGet(1);
            long now = clock.uptimeMs();
            for (int fired : r.timers.fireDue(now)) {
                if (pollIds.contains(fired) && polling) rearmAt = now + 5;
            }
            if (rearmAt != Long.MIN_VALUE && now >= rearmAt) {
                rearmAt = Long.MIN_VALUE;
                arm();
            }
        }

        void advance(long ms) {
            for (long i = 0; i < ms; i++) tick();
        }

        /** Continuous sampling (every 16 ms) for {@code ms}, whatever the counts. */
        List<Integer> sampleFor(long ms) {
            List<Integer> out = new ArrayList<>();
            for (long t = 0; t < ms; t += 16) {
                advance(16);
                out.add(rn.timers(-1));
            }
            return out;
        }

        /** Learn the poll, then advance to just after it fired and re-armed. */
        JsPoll learnt() {
            for (int i = 0; i < 10 && sum(step()) > 0; i++) advance(400);
            int before = pollIds.size();
            while (pollIds.size() == before) tick();
            return this;
        }

        /** In one JS batch: the poll's effect cleanup (clearTimeout) and the next screen's setTimeout(delay). */
        void clearPollAndArm(int id, long delay) {
            polling = false;
            rearmAt = Long.MIN_VALUE;
            r.timers.deleteTimer(pollIds.get(pollIds.size() - 1));
            r.timers.createTimer(id, clock.uptimeMs(), delay, false);
        }

        /** wait_idle: a sample every 16 ms until 4 idle samples in a row (or the 3 s cap). The samples. */
        List<Integer> step() {
            List<Integer> out = new ArrayList<>();
            int quiet = 0;
            for (long t = 0; t < 3_000 && quiet < 4; t += 16) {
                advance(16);
                int n = rn.timers(-1);
                out.add(n);
                quiet = n == 0 ? quiet + 1 : 0;
            }
            return out;
        }
    }

    static int sum(List<Integer> xs) {
        int s = 0;
        for (int x : xs) s += Math.max(0, x);
        return s;
    }

    static void jsPollAcrossSteps() {
        JsPoll p = new JsPoll(1_000);
        // Learn it: the first step(s) wait while the chain builds.
        int learnSteps = 0;
        while (learnSteps < 10 && sum(p.step()) > 0) {
            p.advance(400);
            learnSteps++;
        }
        // Then many steps, with the host acting 300..1000 ms between them (the poll often fires unseen).
        int counted = 0;
        int firedInGap = 0;
        for (int step = 0; step < 60; step++) {
            long gap = 300 + (step * 137) % 700;
            int before = p.pollIds.size();
            p.advance(gap);
            if (p.pollIds.size() != before) firedInGap++;
            counted += sum(p.step());
        }
        check(learnSteps < 10 && counted == 0 && firedInGap > 10,
                "fi4-MAJOR-1: a 1 s JS setTimeout poll is never counted again once it is perpetual, though it fired"
                        + " between steps " + firedInGap + "/60 times (counted " + counted + " samples, learnt in "
                        + learnSteps + " steps)");
        check(p.rn.trackers().get(1).ignoredPerpetual().contains("rn_timers"), "fi4-MAJOR-1: ... reported as ignored perpetual");
    }

    static void jsPollStopsThenOneShot() {
        JsPoll p = new JsPoll(1_000);
        for (int i = 0; i < 10 && sum(p.step()) > 0; i++) p.advance(400);
        // The screen goes away: the poll stops. Steps go on.
        p.polling = false;
        p.advance(1_200);
        p.step();
        p.advance(500);
        p.step();
        // A later one-shot setTimeout(1000) (a toast's dismiss): counted.
        p.advance(500);
        p.r.timers.createTimer(5_000, p.clock.uptimeMs(), 1_000, false);
        List<Integer> s = p.step();
        check(s.size() > 50 && s.get(0) == 1 && s.get(50) == 1,
                "fi4-MAJOR-1: once a poll stopped, a one-shot of its period counts (" + s.subList(0, 3) + "...)");
        // Same after a long gap with nobody asking.
        JsPoll q = new JsPoll(1_000);
        for (int i = 0; i < 10 && sum(q.step()) > 0; i++) q.advance(400);
        q.polling = false;
        q.advance(RepostChains.POSTER_QUIET_MS + 1_500);
        q.r.timers.createTimer(5_000, q.clock.uptimeMs(), 1_000, false);
        List<Integer> s2 = q.step();
        check(s2.get(0) == 1 && s2.size() > 50, "fi4-MAJOR-1: ... and after a quiet gap longer than the expiry");
    }

    static void jsOneShotNextToPoll() {
        JsPoll p = new JsPoll(1_000);
        for (int i = 0; i < 10 && sum(p.step()) > 0; i++) p.advance(400);
        // A one-shot of the poll's own period, created after a gap, while the poll's timer is live.
        boolean always = true;
        for (int k = 0; k < 8; k++) {
            p.advance(300 + 97 * k);
            if (p.rearmAt != Long.MIN_VALUE) p.advance(6); // not mid-round-trip
            p.r.timers.createTimer(9_000 + k, p.clock.uptimeMs(), 1_000, false);
            List<Integer> s = p.step();
            always &= s.size() > 50 && s.get(0) >= 1;
            p.r.timers.deleteTimer(9_000 + k);
        }
        check(always, "fi4-MAJOR-1: a one-shot of the poll's period next to the live poll still counts");
    }

    static void coroutineLoopAcrossSteps() {
        TrackerChecks.Clock clock = new TrackerChecks.Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock);
        TrackerChecks.FakeHandler h = new TrackerChecks.FakeHandler();
        // while (true) { delay(700); poll() } on Main: a new resume instance each time.
        q.enqueue(Message.of(clock.uptimeMs() + 700, h, new TrackerChecks.Shared(700), 0));
        java.util.function.Function<Runnable, Runnable> loop = r -> new TrackerChecks.Shared(700);
        int learn = 0;
        while (learn < 10 && sum(TrackerChecks.frames(clock, q, t, h, 3_000, loop)) > 0) learn++;
        int counted = 0;
        for (int step = 0; step < 40; step++) {
            long gap = 300 + (step * 131) % 700;
            // The host acts: nobody samples, the loop keeps going.
            for (long d = 0; d < gap; d++) {
                clock.now.addAndGet(1);
                for (Runnable r : q.runDue(clock.uptimeMs())) {
                    q.enqueue(Message.of(clock.uptimeMs() + 700, h, loop.apply(r), 0));
                }
            }
            List<Integer> s = TrackerChecks.frames(clock, q, t, h, 100, loop);
            counted += sum(s);
        }
        check(learn < 10 && counted == 0,
                "fi4-MAJOR-1: a coroutine delay() loop on Main stays perpetual across steps (counted " + counted + ")");
    }

    static void looperDebounceRearmedQuickly() {
        // The same save runnable, re-armed by the next tap ~50 ms after it fired, samples throughout.
        TrackerChecks.Clock clock = new TrackerChecks.Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock);
        TrackerChecks.FakeHandler h = new TrackerChecks.FakeHandler();
        Runnable save = TrackerChecks.task();
        boolean always = true;
        for (int tap = 0; tap < 8; tap++) {
            q.enqueue(Message.of(clock.uptimeMs() + 500, h, save, 0));
            List<Integer> f = TrackerChecks.frames(clock, q, t, h, 480, r -> null);
            always &= TrackerChecks.allPositive(f);
            TrackerChecks.frames(clock, q, t, h, 64, r -> null); // fires; wait_idle sees it gone, returns
        }
        check(always, "fi4-m1: a debounce re-armed within 200 ms of firing (samples in between) is never perpetual");
    }

    static int count(List<Integer> xs, int v) {
        int n = 0;
        for (int x : xs) if (x == v) n++;
        return n;
    }

    /** fy2-MAJOR-1 (RN): the poll's clearTimeout and an unrelated setTimeout in one sample: the one-shot counts. */
    static void jsPollClearedThenOneShot() {
        boolean ok = true;
        StringBuilder why = new StringBuilder();
        for (long delay : new long[] {800, 900, 1_000, 1_200}) {
            // After a gap (the host acted; the poll fired unseen in it).
            JsPoll p = new JsPoll(1_000).learnt();
            p.advance(500);
            p.clearPollAndArm(7_000, delay);
            List<Integer> s = p.step();
            boolean gap = s.get(0) == 1 && count(s, 1) >= (delay - 40) / 16;
            // Under continuous sampling (a wait_idle in progress).
            JsPoll c = new JsPoll(1_000).learnt();
            c.sampleFor(320);
            c.clearPollAndArm(7_000, delay);
            List<Integer> s2 = c.sampleFor(delay + 200);
            boolean cont = s2.get(0) == 1 && count(s2, 1) >= (delay - 40) / 16;
            ok &= gap && cont;
            if (!gap || !cont) why.append(" delay ").append(delay).append(": gap ").append(gap).append(" continuous ").append(cont);
        }
        check(ok, "fy2-MAJOR-1: a JS poll cleared (clearTimeout) as an unrelated setTimeout(800..1200) is created in the"
                + " same sample: the one-shot counts until it fires, after a gap and under continuous sampling" + why);
    }

    /** fy2-MAJOR-1: the poll fired unseen and stopped in the same gap as the action's one-shot (out of phase): counted. */
    static void jsPollStoppedInGapThenOneShot() {
        JsPoll p = new JsPoll(1_000).learnt();
        // The poll fires unseen in the gap; its callback finds the screen gone and does not re-arm.
        p.polling = false;
        p.advance(1_300);
        p.r.timers.createTimer(7_100, p.clock.uptimeMs(), 1_000, false);
        p.advance(100);
        List<Integer> s = p.step();
        check(s.get(0) == 1 && count(s, 1) >= 50,
                "fy2-MAJOR-1: a poll that stopped in the gap of the action's setTimeout(1000) (out of phase) does not take it: counted ("
                        + count(s, 1) + " samples)");
    }

    /** fy2-m1: a one-shot due before the poll's re-post (both fresh after a gap) does not take the poller's place. */
    static void jsOneShotDueBeforeRepost() {
        JsPoll p = new JsPoll(1_000).learnt();
        // The poll was re-armed just now (due in ~1000). The action creates a same-period one-shot 600 ms later.
        p.advance(600);
        p.r.timers.createTimer(7_200, p.clock.uptimeMs(), 1_000, false);
        // The poll fires and re-arms unseen; then wait_idle: the one-shot (due ~400 ms out) is first in the queue.
        p.advance(600);
        List<Integer> s = p.step();
        check(s.get(0) == 1 && count(s, 1) >= 20 && s.size() * 16 < 700,
                "fy2-m1: a one-shot due before the poll's re-post counts until it fires, and the poll stays perpetual"
                        + " (no relearning up to the cap): " + s.size() + " samples, " + count(s, 1) + " counted");
    }

    /** fy2-MAJOR-1 (coroutines): a delay(1000) loop's scope is cancelled and another's delay(800) posted in the same sample. */
    static void coroutinePollCancelledThenOneShot() {
        boolean[] results = new boolean[2];
        for (int mode = 0; mode < 2; mode++) {
            TrackerChecks.Clock clock = new TrackerChecks.Clock();
            MessageQueue q = new MessageQueue();
            LooperTimers t = new LooperTimers(q, null, clock);
            TrackerChecks.FakeHandler h = new TrackerChecks.FakeHandler();
            List<Runnable> once = new ArrayList<>();
            Runnable[] cur = {new TrackerChecks.Shared(1_000)};
            q.enqueue(Message.of(clock.uptimeMs() + 1_000, h, cur[0], 0));
            java.util.function.Function<Runnable, Runnable> loop = r -> {
                if (once.contains(r)) return null;
                cur[0] = new TrackerChecks.Shared(1_000);
                return cur[0];
            };
            int learn = 0;
            while (learn < 10 && sum(TrackerChecks.frames(clock, q, t, h, 3_000, loop)) > 0) learn++;
            if (mode == 0) {
                // The host taps "Done" 700 ms later (nobody samples; the loop goes on).
                for (long d = 0; d < 700; d++) {
                    clock.now.addAndGet(1);
                    for (Runnable r : q.runDue(clock.uptimeMs())) q.enqueue(Message.of(clock.uptimeMs() + 1_000, h, loop.apply(r), 0));
                }
            } else {
                TrackerChecks.frames(clock, q, t, h, 320, loop);
            }
            // viewModelScope cancelled (the resume message removed); screen B's init: delay(800).
            q.remove(cur[0]);
            Runnable banner = new TrackerChecks.Shared(800);
            once.add(banner);
            q.enqueue(Message.of(clock.uptimeMs() + 800, h, banner, 0));
            List<Integer> s = TrackerChecks.frames(clock, q, t, h, 1_000, loop);
            results[mode] = learn < 10 && s.get(0) == 1 && count(s, 1) >= 48;
        }
        check(results[0] && results[1],
                "fy2-MAJOR-1: a coroutine delay(1000) loop cancelled as another coroutine's delay(800) is posted: the delay(800)"
                        + " counts until it fires (after a gap " + results[0] + ", continuous " + results[1] + ")");
    }
}
