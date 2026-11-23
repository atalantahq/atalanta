package com.atalanta.sync;

import static com.atalanta.sync.HostChecks.check;

import android.os.Message;
import android.os.MessageQueue;
import com.facebook.react.ReactApplication;
import com.facebook.react.ReactHost;
import com.facebook.react.ReactNativeHost;
import com.facebook.react.animated.NativeAnimatedModule;
import com.facebook.react.bridge.ReactContext;
import com.facebook.react.defaults.DefaultReactHost;
import com.facebook.react.fabric.FabricUIManager;
import com.facebook.react.modules.core.JavaTimerManager;
import com.facebook.react.modules.core.TimingModule;
import com.facebook.react.modules.network.NetworkingModule;
import com.facebook.react.runtime.ReactHostImpl;
import com.facebook.react.runtime.ReactInstance;
import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import okhttp3.Call;
import okhttp3.Interceptor;
import okhttp3.Request;
import okhttp3.Response;

/**
 * Host checks for the counting trackers: main-looper timers (P1), React
 * Native idling (P4) and the OkHttp interceptor (P3). The reflection
 * targets are fakes with the real class and field names
 * (src/hostTest/fakes), so the probe's reflection paths really run.
 */
final class TrackerChecks {
    private TrackerChecks() {}

    /** A Handler stand-in (Message.target). */
    static final class FakeHandler {}

    static final class Clock implements LooperTimers.Clock {
        final AtomicLong now = new AtomicLong(1_000_000);

        @Override
        public long uptimeMs() {
            return now.get();
        }
    }

    static void run() throws Exception {
        timerWalk();
        timerHorizonAndFirstSeen();
        timerViewPostDelayed();
        timerPollerAndDebounce();
        timerSharedClass();
        timerDebounceAfterFiring();
        timerDumpFallback();
        timerWalkDumpRace();
        timerUnknown();
        timerGlobalIgnores();
        timerOverTheWire();
        rnOldArchitecture();
        rnBridgeless();
        rnTimerMethodFallback();
        rnPerpetualTimers();
        rnPerpetualAnimation();
        rnAnimationOnUiThread();
        rnNetworkStream();
        rnLoading();
        rnBroken();
        rnConfig();
        rnNeverCreates();
        rnBeforeStart();
        optOutAndIgnoredOverTheWire();
        okHttpInterceptor();
        okHttpBody();
    }

    /** A one-shot task: a new instance of its own class each call. */
    static Runnable task() {
        return new Task();
    }

    static final class Task implements Runnable {
        @Override
        public void run() {}
    }

    /** One class for every instance (a coroutine's resume lambda, RxAndroid's ScheduledRunnable). */
    static final class Shared extends Poller {
        Shared() {
            super(400);
        }

        Shared(long period) {
            super(period);
        }
    }

    /** A callback that re-posts itself (the same instance) {@code period} ms after each run. */
    static class Poller implements Runnable {
        final long period;

        Poller(long period) {
            this.period = period;
        }

        @Override
        public void run() {}
    }

    /**
     * Advance {@code ms} in 16 ms frames (what wait_idle samples at): run the
     * due messages, re-post pollers, sample. Returns the samples.
     */
    static List<Integer> frames(Clock clock, MessageQueue q, LooperTimers t, Object h, long ms,
            java.util.function.Function<Runnable, Runnable> repost) {
        List<Integer> out = new ArrayList<>();
        for (long done = 0; done < ms; done += 16) {
            clock.now.addAndGet(16);
            long now = clock.uptimeMs();
            for (Runnable r : q.runDue(now)) {
                Runnable again = repost.apply(r);
                if (again != null) {
                    long period = again instanceof Poller ? ((Poller) again).period : 400;
                    q.enqueue(Message.of(now + period, h, again, 0));
                }
            }
            out.add(t.pending(-1));
        }
        return out;
    }

    static int last(List<Integer> xs) {
        return xs.get(xs.size() - 1);
    }

    static boolean allPositive(List<Integer> xs) {
        for (int x : xs) if (x <= 0) return false;
        return true;
    }

    static void timerWalk() {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, out -> {
            for (String l : q.dump(clock.uptimeMs())) out.line(l);
        }, clock);
        long now = clock.uptimeMs();
        check(t.pending(-1) == 0, "P1: an empty main queue has no timers");
        FakeHandler h = new FakeHandler();
        q.enqueue(Message.of(now + 500, h, task(), 0));
        check(t.pending(-1) == 1 && t.walking(), "P1: postDelayed(500 ms) is a pending timer (MessageQueue walk)");
        q.enqueue(Message.of(now, h, task(), 0));
        q.enqueue(Message.of(now - 5, h, null, 7));
        check(t.pending(-1) == 1, "P1: messages due now are the main_looper check's, not timers");
        q.enqueue(Message.of(now + 300, null, null, 0));
        check(t.pending(-1) == 1, "P1: a sync barrier is not a timer");
        q.enqueue(Message.of(now + 200, h, null, 42));
        check(t.pending(-1) == 2, "P1: sendMessageDelayed (what, no callback) counts too");
        clock.now.addAndGet(600);
        q.runDue(clock.uptimeMs());
        check(t.pending(-1) == 0, "P1: idle once the delayed messages ran");
    }

    static void timerHorizonAndFirstSeen() {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock);
        long now = clock.uptimeMs();
        FakeHandler h = new FakeHandler();
        q.enqueue(Message.of(now + 800, h, task(), 0));
        check(t.pending(-1) == 1, "P1: 0.8 s is within the default 1.5 s horizon");
        check(t.pending(100) == 0, "P1: max_timer_ms 100 ignores a 0.8 s timer");
        q.clear();
        q.enqueue(Message.of(now + 3_000, h, task(), 0));
        check(t.pending(-1) == 0, "P1: a 3 s timer is past the default horizon");
        check(t.pending(5_000) == 1, "P1: max_timer_ms 5000 waits for a 3 s timer");
        q.clear();
        Runnable timeout = task();
        q.enqueue(Message.of(now + 10_000, h, timeout, 0));
        check(t.pending(-1) == 0, "P1: a 10 s timeout is not short work");
        clock.now.addAndGet(9_200);
        check(t.pending(-1) == 0, "P1: ... nor once it is 0.8 s from firing (first seen 10 s out, as iOS)");
    }

    static void timerViewPostDelayed() {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, out -> {
            for (String l : q.dump(clock.uptimeMs())) out.line(l);
        }, clock);
        // View.postDelayed -> AttachInfo.mHandler, a ViewRootImpl$ViewRootHandler.
        Object viewRootHandler = new android.view.ViewRootImpl().mHandler;
        check(viewRootHandler.getClass().getName().equals("android.view.ViewRootImpl$ViewRootHandler"), "B1: fake has the real name");
        q.enqueue(Message.of(clock.uptimeMs() + 400, viewRootHandler, task(), 0));
        check(t.pending(-1) == 1, "B1: View.postDelayed (target ViewRootImpl$ViewRootHandler) is a pending timer");
        LooperTimers dumpOnly = new LooperTimers(null, out -> {
            for (String l : q.dump(clock.uptimeMs())) out.line(l);
        }, clock);
        check(dumpOnly.pending(-1) == 1, "B1: ... in dump mode too");
        // The ignore list matches callbacks; a target only for a message without one.
        q.clear();
        q.enqueue(Message.of(clock.uptimeMs() + 300, new FakeHandler(), new BlinkLike(), 0));
        q.enqueue(Message.of(clock.uptimeMs() + 300, new FakeHandler(), null, 5));
        t.ignoreCallbacks(Collections.singletonList("com.atalanta.sync.TrackerChecks$FakeHandler"));
        check(t.pending(-1) == 1, "B1: an ignored target class drops its what-messages, not the callbacks posted through it");
    }

    static void timerPollerAndDebounce() {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock);
        FakeHandler h = new FakeHandler();
        // A poller: the same instance re-posts itself 500 ms after each run.
        Poller poll = new Poller(500);
        q.enqueue(Message.of(clock.uptimeMs() + 500, h, poll, 0));
        List<Integer> s = frames(clock, q, t, h, 3_000, r -> r == poll ? poll : null);
        check(allPositive(s.subList(0, 30)) && last(s) == 0 && t.ignoredPerpetual().contains("timer"),
                "M1/M3: a callback that re-posts itself as it fires counts at first, then is perpetual (ignored)");
        // A debounce: removed and posted again before it fires, every 100 ms; counts on every sample.
        Runnable debounce = task();
        boolean always = true;
        for (int i = 0; i < 30; i++) {
            q.remove(debounce);
            q.enqueue(Message.of(clock.uptimeMs() + 300, h, debounce, 0));
            List<Integer> f = frames(clock, q, t, h, 96, r -> r == poll ? poll : null);
            always &= allPositive(f) && t.pending(-1) == 1;
        }
        check(always, "M1: a debounce restarted on each keystroke counts on every sample, next to a poller");
        frames(clock, q, t, h, 400, r -> r == poll ? poll : null);
        check(t.pending(-1) == 0, "M1: ... and is idle once it fired (the poller still ignored)");
        // The cursor blink is ignored by default; apps add their own.
        q.clear();
        q.enqueue(Message.of(clock.uptimeMs() + 400, h, new BlinkLike(), 0));
        check(t.pending(-1) == 1, "P1: an app callback counts");
        t.ignoreCallbacks(Collections.singletonList("com.atalanta.sync.TrackerChecks$BlinkLike"));
        check(t.pending(-1) == 0, "P1: ignoreTimerCallbacks drops it");
    }

    static void timerSharedClass() {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock);
        FakeHandler h = new FakeHandler();
        // A coroutine delay(400) loop on Main: a new Shared instance each time.
        q.enqueue(Message.of(clock.uptimeMs() + 400, h, new Shared(), 0));
        java.util.function.Function<Runnable, Runnable> loop = r -> r instanceof Shared ? reposted(r) : null;
        List<Integer> s = frames(clock, q, t, h, 4_000, loop);
        check(allPositive(s.subList(0, 100)) && last(s) == 0,
                "M1: a coroutine delay() loop (new instance, one class) counts for 2 s, then is perpetual (" + s.subList(0, 5) + "..." + last(s) + ")");
        // Another coroutine's one-shot delay(250) on Main: same class, not part of the loop.
        Shared once = new Shared();
        q.enqueue(Message.of(clock.uptimeMs() + 250, h, once, 0));
        oneShots.add(once);
        List<Integer> f = frames(clock, q, t, h, 200, loop);
        check(allPositive(f), "M1: a one-shot of the poller's class still counts (" + f + ")");
        // Sequential one-shots in one coroutine: delay(500) three times.
        Clock c2 = new Clock();
        MessageQueue q2 = new MessageQueue();
        LooperTimers t2 = new LooperTimers(q2, null, c2);
        q2.enqueue(Message.of(c2.uptimeMs() + 500, h, new Shared(500), 0));
        int[] left = {2};
        List<Integer> seq = frames(c2, q2, t2, h, 1_480, r -> left[0]-- > 0 ? new Shared(500) : null);
        check(allPositive(seq), "M1: three sequential delay(500)s in one coroutine all count (" + seq + ")");
    }

    static final List<Runnable> oneShots = new ArrayList<>();

    /** The loop's next resume: a new Shared (one-shots don't re-post). */
    static Runnable reposted(Runnable r) {
        return oneShots.contains(r) ? null : new Shared();
    }


    static void timerDebounceAfterFiring() {
        // The same save runnable posted (500 ms) by a tap every ~1 s: re-armed after it fired.
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock);
        FakeHandler h = new FakeHandler();
        Runnable save = task();
        boolean always = true;
        for (int tap = 0; tap < 6; tap++) {
            q.remove(save);
            q.enqueue(Message.of(clock.uptimeMs() + 500, h, save, 0));
            List<Integer> f = frames(clock, q, t, h, 480, r -> null); // wait_idle samples
            always &= allPositive(f);
            frames(clock, q, t, h, 100, r -> null);
            clock.now.addAndGet(400); // no samples until the next tap
        }
        check(always, "M1: a debounce re-armed after it fired counts on every tap");
        // Same, a tap exactly as the save fires (no sample saw it gone), twice: still counted.
        always = true;
        for (int tap = 0; tap < 4; tap++) {
            q.enqueue(Message.of(clock.uptimeMs() + 500, h, save, 0));
            List<Integer> f = frames(clock, q, t, h, 480, r -> null);
            always &= allPositive(f);
            clock.now.addAndGet(700); // a step without samples
        }
        check(always, "M1: re-armed across steps without samples, a one-shot is never promoted to a poller");
    }
    static final class BlinkLike implements Runnable {
        @Override
        public void run() {}
    }

    /** A queue class whose list isn't where the probe looks (another implementation). */
    static final class OtherQueue {
        Object mMessages; // always empty: messages live elsewhere
    }

    static void timerDumpFallback() {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        long now = clock.uptimeMs();
        FakeHandler h = new FakeHandler();
        q.enqueue(Message.of(now + 700, h, task(), 0));
        q.enqueue(Message.of(now + 1_200, h, null, 3));
        q.enqueue(Message.of(now + 9_000, h, task(), 0));
        LooperTimers dumpOnly = new LooperTimers(null, out -> {
            for (String l : q.dump(clock.uptimeMs())) out.line(l);
        }, clock);
        check(dumpOnly.pending(-1) == 2 && !dumpOnly.walking(), "P1: Looper.dump fallback reads the same timers");
        check(dumpOnly.pending(1_000) == 1, "P1: max_timer_ms applies to the dump fallback");

        LooperTimers disagree = new LooperTimers(new OtherQueue(), out -> {
            for (String l : q.dump(clock.uptimeMs())) out.line(l);
        }, clock);
        int n = disagree.pending(-1);
        check(n == 2 && disagree.walking(),
                "P1/m1: a walk that sees nothing while the dump has messages is not trusted (" + n + "), one disagreement doesn't switch");
        clock.now.addAndGet(16);
        n = disagree.pending(-1);
        check(n == 2 && !disagree.walking(), "m1: a second disagreement in a row switches to the dump (" + n + ")");
        clock.now.addAndGet(LooperTimers.DUMP_RETRY_MS + 1);
        check(disagree.walking(), "m1: ... for a while, not for good: the walk is tried again");

        check(LooperTimers.parseDuration("+1s500ms") == 1_500 && LooperTimers.parseDuration("-12ms") == -12
                && LooperTimers.parseDuration("0") == 0 && LooperTimers.parseDuration("+1m2s3ms") == 62_003
                && LooperTimers.parseDuration("+2h") == 7_200_000
                && LooperTimers.parseDuration("soon") == Long.MIN_VALUE
                && LooperTimers.parseDuration("+5x") == Long.MIN_VALUE,
                "P1: TimeUtils.formatDuration strings parse");
    }

    static void timerWalkDumpRace() {
        // A message posted between the (empty) walk and the dump is not a disagreement.
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        int[] dumps = {0};
        LooperTimers t = new LooperTimers(q, out -> {
            dumps[0]++;
            q.enqueue(Message.of(clock.uptimeMs() + 300, new FakeHandler(), task(), 0)); // the race
            for (String l : q.dump(clock.uptimeMs())) out.line(l);
        }, clock);
        int n = t.pending(-1);
        clock.now.addAndGet(LooperTimers.VERIFY_MS + 1);
        q.clear();
        t.pending(-1);
        check(dumps[0] == 2 && t.walking() && n == 1,
                "m1: a message posted between the walk and the dump keeps the walk (re-walked), never a permanent dump (" + n + ")");
    }

    static void timerGlobalIgnores() {
        LooperTimers.ignoreCallbacksGlobally(Collections.singletonList("com.atalanta.sync.TrackerChecks$Blink2"));
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        LooperTimers t = new LooperTimers(q, null, clock); // made after (start() after ignoreTimerCallbacks)
        q.enqueue(Message.of(clock.uptimeMs() + 300, new FakeHandler(), new Blink2(), 0));
        check(t.pending(-1) == 0, "m3: ignoreTimerCallbacks before start (or across stop/start) takes effect");
        LooperTimers.clearGlobalIgnores();
        check(t.pending(-1) == 1, "m3: ... and only for those prefixes");
    }

    static final class Blink2 implements Runnable {
        @Override
        public void run() {}
    }

    static void timerUnknown() {
        Clock clock = new Clock();
        LooperTimers none = new LooperTimers(new Object(), null, clock);
        check(none.pending(-1) == Tracker.UNKNOWN, "P1: no walk and no dump is unknown, not 0");
        LooperTimers throwing = new LooperTimers(null, out -> {
            throw new SecurityException("no");
        }, clock);
        check(throwing.pending(-1) == Tracker.UNKNOWN, "P1: a dump that throws is unknown");
        Trackers tr = new Trackers();
        tr.add(none);
        check(tr.idle(-1) && tr.unknown().equals(Collections.singletonList("timer")),
                "P1: an unknown timer tracker is reported in unknown, not as pending");
        check(ProbeLog.warned("timer-unknown"), "P1: ... with one warning");
        Trackers boom = new Trackers();
        boom.add(new Tracker() {
            public String name() { return "boom"; }
            public int pending(long m) { throw new IllegalStateException("x"); }
        });
        check(boom.idle(-1) && boom.unknown().contains("boom"), "P1: a tracker that throws is unknown, never a crash");
    }

    /** ProbeState over a Trackers instance, as the Kotlin AppStateTracker composes it. */
    static final class TrackerState implements ProbeState {
        final Trackers trackers;

        TrackerState(Trackers trackers) {
            this.trackers = trackers;
        }

        public boolean idleNow() { return idleNow(-1); }
        public List<String> pendingNow() { return pendingNow(-1); }
        @Override public boolean idleNow(long maxTimerMs) { return trackers.idle(maxTimerMs); }
        @Override public List<String> pendingNow(long maxTimerMs) { return trackers.pending(maxTimerMs); }
        public List<String> unknownTrackers() { return trackers.unknown(); }
        @Override public boolean enableFramework(String f) { return trackers.enableFramework(f); }
        @Override public List<String> ignoredNow() { return trackers.ignored(); }
        @Override public List<String> optOutTrackers(List<String> names) { return trackers.optOut(names); }
    }

    static void timerOverTheWire() throws Exception {
        Clock clock = new Clock();
        MessageQueue q = new MessageQueue();
        q.enqueue(Message.of(clock.uptimeMs() + 800, new FakeHandler(), task(), 0));
        Trackers tr = new Trackers();
        tr.add(new LooperTimers(q, null, clock));
        SyncServer server = new SyncServer(new TrackerState(tr), 0, "com.example.app", 7);
        server.start();
        check(server.awaitBound(2000), "P1: server binds");
        int port = server.localPort();
        String status = HostChecks.ask("127.0.0.1", port, "{\"cmd\":\"status\"}");
        check(status != null && status.contains("\"idle\":false") && status.contains("timer:1"),
                "P1: a 0.8 s postDelayed is pending over the wire (" + status + ")");
        List<String> short_ = HostChecks.askAll(port, "{\"cmd\":\"config\",\"max_timer_ms\":100}",
                "{\"cmd\":\"wait_idle\",\"timeout_ms\":300}");
        check(short_.size() == 2 && short_.get(1) != null && short_.get(1).contains("\"idle\":true"),
                "P1: max_timer_ms now applies on Android (" + short_ + ")");
        String hello = HostChecks.ask("127.0.0.1", port, "{\"cmd\":\"hello\"}");
        check(hello != null && hello.contains("\"version\":4"), "P1/P4: protocol 4 (" + hello + ")");
        server.stop();
    }

    // --- React Native ---------------------------------------------------------

    static final class OldArchApp implements ReactApplication {
        final ReactNativeHost host = new ReactNativeHost();

        @Override
        public ReactNativeHost getReactNativeHost() {
            return host;
        }
    }

    /** The RN template's bridgeless app: getReactHost() is DefaultReactHost.getDefaultReactHost(...). */
    static final class BridgelessApp implements ReactApplication {
        final ReactHostImpl host = new ReactHostImpl();

        @Override
        public ReactNativeHost getReactNativeHost() {
            throw new UnsupportedOperationException("bridgeless");
        }

        @Override
        public ReactHost getReactHost() {
            return DefaultReactHost.getDefaultReactHost(() -> host);
        }
    }

    static final class Rn {
        final JavaTimerManager timers = new JavaTimerManager();
        final FabricUIManager fabric = new FabricUIManager();
        final NativeAnimatedModule animated = new NativeAnimatedModule();
        final NetworkingModule networking = new NetworkingModule();
        final ReactContext ctx = new ReactContext();

        Rn(boolean timingModule) {
            if (timingModule) ctx.modules.put(TimingModule.class, new TimingModule(timers));
            ctx.modules.put(NativeAnimatedModule.class, animated);
            ctx.modules.put(NetworkingModule.class, networking);
            ctx.fabric = fabric;
        }
    }

    static int byName(List<Tracker> ts, String name, long max) {
        for (Tracker t : ts) if (t.name().equals(name)) return t.pending(max);
        return -99;
    }

    static Tracker named(List<Tracker> ts, String name) {
        for (Tracker t : ts) if (t.name().equals(name)) return t;
        return null;
    }

    static void rnOldArchitecture() {
        OldArchApp app = new OldArchApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS), clock);
        List<Tracker> ts = rn.trackers();
        check(ReactNativeIdling.isReactApplication(app) && !ReactNativeIdling.isReactApplication(new Object()),
                "P4: ReactApplication is detected by reflection");
        check(byName(ts, "rn_loading", -1) == 1 && app.host.created == 0,
                "P4: no React context yet is rn_loading, and the probe never creates the instance manager");
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        check(byName(ts, "rn_loading", -1) == 0, "P4: loaded");
        check(byName(ts, "rn_timers", -1) == 0, "P4: no JS timers is idle");
        r.timers.createTimer(1, clock.uptimeMs(), 500, false);
        check(byName(ts, "rn_timers", -1) == 1, "P4/M2: setTimeout(500) is pending (JavaTimerManager.timers)");
        check(byName(ts, "rn_timers", 100) == 0, "P4: max_timer_ms is the range (interval < range, RN's rule)");
        r.timers.clear();
        r.timers.createTimer(2, clock.uptimeMs(), 100, true);
        check(byName(ts, "rn_timers", -1) == 0, "P4: setInterval doesn't count (RN's rule)");
        r.timers.clear();

        r.fabric.dispatcher().addMountItem("a");
        r.fabric.dispatcher().addViewCommand("b");
        r.fabric.dispatcher().addPreMount("c");
        check(byName(ts, "rn_mounting", -1) == 2, "P4: Fabric mount items and view commands are pending (pre-mount not)");
        r.fabric.dispatcher().dispatchAll();
        check(byName(ts, "rn_mounting", -1) == 0, "P4: idle once mounted");

        check(byName(ts, "rn_animation", -1) == 0 && r.animated.created == 0,
                "P4: no nodes manager yet is idle, and the probe doesn't create one");
        r.animated.getNodesManager().start(1, 1);
        check(byName(ts, "rn_animation", -1) == 1,
                "P4/M2: a native-driven animation is pending (RN 0.81 NativeAnimatedModule.nodesManagerRef)");
        r.animated.getNodesManager().finish(1);
        check(byName(ts, "rn_animation", -1) == 0, "P4: idle once it ended");

        r.networking.client().dispatcher().running.add(new Call("https://api.example.com/items"));
        r.networking.client().dispatcher().running.add(new Call("wss://chat.example.com/live"));
        check(byName(ts, "rn_network", -1) == 1, "P4: a running fetch is pending, a WebSocket is not");
        r.networking.client().dispatcher().queued.add(new Call("https://api.example.com/more"));
        check(byName(ts, "rn_network", -1) == 2, "P4: queued calls count");
        r.networking.client().dispatcher().running.clear();
        r.networking.client().dispatcher().queued.clear();
        check(byName(ts, "rn_network", -1) == 0, "P4: idle once they finished");
    }

    static void rnBridgeless() {
        DefaultReactHost.reset();
        BridgelessApp app = new BridgelessApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        List<Tracker> ts = rn.trackers();
        check(byName(ts, "rn_loading", -1) == 1 && DefaultReactHost.created == 0,
                "P4/m5: bridgeless, no ReactHost yet is loading, and the probe doesn't create it");
        app.getReactHost(); // ReactActivity.onCreate makes it
        check(byName(ts, "rn_loading", -1) == 1, "P4: host made, no context yet: loading");
        Rn r = new Rn(false); // no TimingModule: bridgeless's CoreReactPackage doesn't register it
        app.host.context = r.ctx;
        app.host.setInstance(new ReactInstance(r.timers));
        r.timers.createTimer(7, clock.uptimeMs(), 200, false);
        check(byName(ts, "rn_loading", -1) == 0 && byName(ts, "rn_timers", -1) == 1,
                "M2: bridgeless JS timers through ReactHostImpl.mReactInstance.javaTimerManager (no TimingModule)");
        r.ctx.fabric = null; // getFabricUIManager() null: UIManagerHelper path
        check(byName(ts, "rn_mounting", -1) == Tracker.UNKNOWN,
                "P4: no Fabric manager and no UIManagerModule is unknown, not idle and not a crash");
        DefaultReactHost.reset();
    }

    static void rnTimerMethodFallback() {
        // The queue unreadable (another RN): the Kotlin-internal, mangled method.
        DefaultReactHost.reset();
        BridgelessApp app = new BridgelessApp();
        app.getReactHost();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        Rn r = new Rn(false);
        app.host.context = r.ctx;
        app.host.setInstance(new ReactInstance(r.timers));
        r.timers.hideQueue();
        r.timers.createHidden(1, clock.uptimeMs(), 300, false);
        check(rn.timers(-1) == 1 && r.timers.lastRange == LooperTimers.DEFAULT_MAX_TIMER_MS,
                "M2: hasActiveTimersInRange$ReactAndroid_release (Kotlin internal) is found by its name prefix");
        r.timers.clearHidden();
        check(rn.timers(-1) == 0, "M2: ... and idle when it says so");
        // Busy without a break for 2x the horizon: timers re-armed again and again.
        r.timers.createHidden(2, clock.uptimeMs(), 300, false);
        int lastN = -1;
        for (int i = 0; i < 250; i++) {
            clock.now.addAndGet(16);
            lastN = rn.timers(-1);
        }
        check(lastN == 0 && named(rn.trackers(), "rn_timers").ignoredPerpetual().contains("rn_timers"),
                "M3: busy without a break (no timer ids to tell) stops counting after 2x the horizon");
        r.timers.clearHidden();
        check(rn.timers(-1) == 0 && named(rn.trackers(), "rn_timers").ignoredPerpetual().isEmpty(), "M3: ... reset once idle");
        DefaultReactHost.reset();
    }

    static void rnPerpetualTimers() {
        OldArchApp app = new OldArchApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        // A recursive setTimeout(poll, 500): each timer re-armed (new id) as it fires.
        int[] id = {100};
        r.timers.createTimer(id[0]++, clock.uptimeMs(), 500, false);
        List<Integer> s = new ArrayList<>();
        for (int i = 0; i < 250; i++) {
            clock.now.addAndGet(16);
            for (int ignored : r.timers.fireDue(clock.uptimeMs())) {
                if (ignored < 1000) r.timers.createTimer(id[0]++, clock.uptimeMs(), 500, false);
            }
            s.add(rn.timers(-1));
        }
        check(allPositive(s.subList(0, 90)) && last(s) == 0
                        && named(rn.trackers(), "rn_timers").ignoredPerpetual().contains("rn_timers"),
                "M3: a recursive setTimeout poll counts at first, then is ignored as perpetual");
        // A one-shot setTimeout(300) next to it still counts.
        r.timers.createTimer(5000, clock.uptimeMs(), 300, false);
        List<Integer> f = new ArrayList<>();
        for (int i = 0; i < 15; i++) {
            clock.now.addAndGet(16);
            for (int fired : r.timers.fireDue(clock.uptimeMs())) {
                if (fired < 1000) r.timers.createTimer(id[0]++, clock.uptimeMs(), 500, false);
            }
            f.add(rn.timers(-1));
        }
        check(allPositive(f), "M3: a one-shot JS timer next to a poll still counts (" + f + ")");
        // A debounce: clearTimeout + setTimeout(300) on every keystroke counts throughout.
        Rn d = new Rn(true);
        OldArchApp app2 = new OldArchApp();
        app2.host.getReactInstanceManager().context = d.ctx;
        ReactNativeIdling rn2 = new ReactNativeIdling(app2, UrlBlocklist.SHARED, clock);
        List<Integer> deb = new ArrayList<>();
        for (int i = 0; i < 200; i++) {
            if (i % 6 == 0) {
                d.timers.deleteTimer(i - 6);
                d.timers.createTimer(i, clock.uptimeMs(), 300, false);
            }
            clock.now.addAndGet(16);
            d.timers.fireDue(clock.uptimeMs());
            deb.add(rn2.timers(-1));
        }
        check(allPositive(deb), "M3: a JS debounce re-armed on each keystroke for 3 s counts throughout");
        // requestAnimationFrame loop: a 1 ms timer, re-armed every frame.
        Rn a = new Rn(true);
        OldArchApp app3 = new OldArchApp();
        app3.host.getReactInstanceManager().context = a.ctx;
        ReactNativeIdling rn3 = new ReactNativeIdling(app3, UrlBlocklist.SHARED, clock);
        a.timers.createTimer(1, clock.uptimeMs(), 1, false);
        int n = 1;
        List<Integer> raf = new ArrayList<>();
        for (int i = 0; i < 200; i++) {
            clock.now.addAndGet(16);
            for (int ignored : a.timers.fireDue(clock.uptimeMs())) a.timers.createTimer(++n, clock.uptimeMs(), 1, false);
            raf.add(rn3.timers(-1));
        }
        check(raf.get(5) == 1 && last(raf) == 0, "M3: a requestAnimationFrame loop stops counting after ~2 s");
    }

    static void rnPerpetualAnimation() {
        OldArchApp app = new OldArchApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        com.facebook.react.animated.NativeAnimatedNodesManager nodes = r.animated.getNodesManager();
        // Animated.loop(..., {useNativeDriver: true}): iterations -1 on the native driver.
        nodes.start(1, -1).loopAgain();
        nodes.touchNode(9, true);
        List<Integer> s = new ArrayList<>();
        for (int i = 0; i < 160; i++) {
            clock.now.addAndGet(16);
            s.add(rn.animation());
        }
        check(allPositive(s.subList(0, 120)) && last(s) == 0
                        && named(rn.trackers(), "rn_animation").ignoredPerpetual().contains("rn_animation"),
                "M3: a looping native animation counts for 2 s, then is ignored as perpetual");
        nodes.start(2, 1); // a one-shot fade on top
        clock.now.addAndGet(16);
        check(rn.animation() == 1, "M3: a one-shot animation next to the loop still counts");
        nodes.finish(2);
        nodes.finish(1);
        nodes.touchNode(9, false);
        clock.now.addAndGet(16);
        check(rn.animation() == 0 && named(rn.trackers(), "rn_animation").ignoredPerpetual().isEmpty(), "M3: idle, nothing ignored, once it stopped");
        // A JS-restarted loop: a new driver each iteration (300 ms), without a break.
        int id = 10;
        nodes.start(id, 1);
        List<Integer> js = new ArrayList<>();
        for (int i = 0; i < 200; i++) {
            clock.now.addAndGet(16);
            if (i % 19 == 18) {
                nodes.finish(id);
                nodes.start(++id, 1);
            }
            js.add(rn.animation());
        }
        check(js.get(10) == 1 && last(js) == 0, "M3: an animation restarted by JS again and again stops counting");
        // A single long one-shot (3 s) is not a loop: counts throughout.
        nodes.finish(id);
        clock.now.addAndGet(16);
        rn.animation();
        nodes.start(99, 1);
        List<Integer> one = new ArrayList<>();
        for (int i = 0; i < 180; i++) {
            clock.now.addAndGet(16);
            one.add(rn.animation());
        }
        check(allPositive(one), "M3: a long one-shot native animation counts until it ends");
        // Animated.loop(..., {iterations: 3}) of 1 s each: finite, counts until it ends.
        nodes.finish(99);
        clock.now.addAndGet(16);
        rn.animation();
        com.facebook.react.animated.FrameBasedAnimationDriver three = nodes.start(200, 3);
        List<Integer> fin = new ArrayList<>();
        for (int i = 0; i < 186; i++) {
            clock.now.addAndGet(16);
            if (i == 62 || i == 124) three.loopAgain();
            fin.add(rn.animation());
        }
        check(allPositive(fin) && !named(rn.trackers(), "rn_animation").ignoredPerpetual().contains("rn_animation"),
                "fi4-m5: a finite Animated.loop (iterations 3) counts through its last iteration");
        nodes.finish(200);
    }

    static void rnAnimationOnUiThread() throws Exception {
        java.util.concurrent.ExecutorService ui = java.util.concurrent.Executors.newSingleThreadExecutor();
        Thread uiThread = ui.submit(Thread::currentThread).get();
        android.util.SparseArray.owner = uiThread;
        android.util.SparseArray.offThreadReads.set(0);
        try {
            OldArchApp app = new OldArchApp();
            Clock clock = new Clock();
            ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock, r -> {
                ui.execute(r);
                return true;
            });
            Rn r = new Rn(true);
            app.host.getReactInstanceManager().context = r.ctx;
            ui.submit(() -> r.animated.getNodesManager().start(1, 1)).get();
            int busy = rn.animation();
            ui.submit(() -> r.animated.getNodesManager().finish(1)).get();
            clock.now.addAndGet(16);
            int idle = rn.animation();
            check(busy == 1 && idle == 0 && android.util.SparseArray.offThreadReads.get() == 0,
                    "fi4-m4: NativeAnimatedNodesManager.activeAnimations (a SparseArray) is read on the UI thread only ("
                            + android.util.SparseArray.offThreadReads.get() + " reads off it)");
            // A UI thread that doesn't answer: the last reading, not a read from the probe's thread.
            ui.submit(() -> r.animated.getNodesManager().start(2, 1)).get();
            clock.now.addAndGet(16);
            rn.animation();
            ReactNativeIdling stuck = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock, x -> false);
            int s = stuck.animation();
            check(s == 0 && android.util.SparseArray.offThreadReads.get() == 0,
                    "fi4-m4: ... and when the UI thread can't answer, nothing is read from the probe's thread");
        } finally {
            android.util.SparseArray.owner = null;
            ui.shutdownNow();
        }
    }

    static void rnNetworkStream() {
        OldArchApp app = new OldArchApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        r.networking.client().dispatcher().running.add(new Call("https://api.example.com/events"));
        check(rn.network() == 1, "M3: a fetch counts");
        clock.now.addAndGet(ReactNativeIdling.STREAM_MAX_MS + 1);
        check(rn.network() == 0 && named(rn.trackers(), "rn_network").ignoredPerpetual().contains("rn_network"),
                "M3: a fetch open longer than the stream bound stops counting, reported as perpetual");
        r.networking.client().dispatcher().running.add(new Call("https://api.example.com/items"));
        check(rn.network() == 1, "M3: a new request next to the stream counts");
    }

    static void rnLoading() {
        OldArchApp app = new OldArchApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        check(rn.loading(-1) == 1, "P4: loading counts at first");
        clock.now.addAndGet(ReactNativeIdling.LOADING_MAX_MS + 1);
        check(rn.loading(-1) == Tracker.UNKNOWN, "P4: a context that never comes is unknown after 30 s, not busy forever");
        // Brownfield / reload: the window starts when the context went missing, not at config.
        OldArchApp later = new OldArchApp();
        ReactNativeIdling rn2 = new ReactNativeIdling(later, UrlBlocklist.SHARED, clock);
        Rn r = new Rn(true);
        later.host.getReactInstanceManager().context = r.ctx;
        check(rn2.loading(-1) == 0, "m4: loaded");
        clock.now.addAndGet(ReactNativeIdling.LOADING_MAX_MS * 2);
        later.host.getReactInstanceManager().context = null; // DevSettings.reload / CodePush restart
        check(rn2.loading(-1) == 1, "m4: a reload 60 s after config is loading, not unknown");
    }

    static void rnBroken() {
        OldArchApp app = new OldArchApp();
        app.host.getReactInstanceManager().context = new ReactContext(); // no modules, no Fabric
        Trackers tr = new Trackers();
        tr.setApplication(app);
        check(tr.enableFramework("react_native") && tr.reactNativeEnabled(), "P4: framework react_native turns tracking on");
        List<String> unknown = tr.unknown();
        check(tr.idle(-1) && unknown.containsAll(Arrays.asList("rn_timers", "rn_mounting"))
                        && !unknown.contains("rn_loading"),
                "P4: an RN without these internals is unknown per tracker, not idle work (" + unknown + ")");
        check(ProbeLog.warned("rn-rn_timers") && ProbeLog.warned("rn-rn_mounting"), "P4: each warned once");

        Trackers plain = new Trackers();
        plain.setApplication(new Object());
        plain.enableFramework("react_native");
        check(plain.unknown().contains("rn_loading") && ProbeLog.warned("rn-app"),
                "P4: react_native on an app that isn't a ReactApplication says so");
        check(!new Trackers().enableFramework("flutter"), "P4: an unknown framework is refused");
    }

    static void rnConfig() {
        OldArchApp app = new OldArchApp();
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        Trackers tr = new Trackers();
        tr.setApplication(app);
        SyncServer server = new SyncServer(new TrackerState(tr), 0, "com.example.app", 7);
        SyncServer.Session session = new SyncServer.Session();
        String before = server.processCommand("{\"cmd\":\"status\"}", session);
        r.fabric.dispatcher().addMountItem("x");
        String still = server.processCommand("{\"cmd\":\"status\"}", session);
        check(before.contains("\"idle\":true") && still.contains("\"idle\":true"),
                "P4: RN tracking is off until config says react_native (" + still + ")");
        String cfg = server.processCommand("{\"cmd\":\"config\",\"framework\":\"react_native\"}", session);
        check(cfg.contains("\"ok\":true") && cfg.contains("\"framework\":\"react_native\"") && cfg.contains("\"unknown\":[]"),
                "P4: config takes framework (" + cfg + ")");
        String busy = server.processCommand("{\"cmd\":\"status\"}", session);
        check(busy.contains("\"idle\":false") && busy.contains("rn_mounting:1"), "P4: then mounting is pending (" + busy + ")");
        String other = server.processCommand("{\"cmd\":\"config\",\"framework\":\"flutter\"}", new SyncServer.Session());
        check(other.contains("\"framework\":null"), "P4: a framework the probe can't track is null (" + other + ")");
        String none = server.processCommand("{\"cmd\":\"config\",\"max_timer_ms\":10}", new SyncServer.Session());
        check(!none.contains("framework"), "P4: a config without framework replies as before (" + none + ")");
    }

    static void rnNeverCreates() {
        // Registered but not yet made modules (lazy TurboModules): the probe never makes them.
        OldArchApp app = new OldArchApp();
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        ReactContext ctx = new ReactContext();
        NativeAnimatedModule animated = new NativeAnimatedModule();
        ctx.lazy.put(NativeAnimatedModule.class, () -> animated);
        ctx.lazy.put(NetworkingModule.class, NetworkingModule::new);
        app.host.getReactInstanceManager().context = ctx;
        check(rn.animation() == 0 && rn.network() == 0 && ctx.createdLazily == 0,
                "m5: a native module not made yet is idle, and the probe doesn't make it (getNativeModule would)");
        ctx.getNativeModule(NativeAnimatedModule.class).getNodesManager().start(1, 1);
        check(rn.animation() == 1, "m5: once the app made it, it is read");
    }

    static void rnBeforeStart() {
        // trackReactNative() before start(context): no Application yet.
        Trackers tr = new Trackers();
        tr.enableFramework("react_native");
        OldArchApp app = new OldArchApp();
        tr.setApplication(app); // start(context)
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        r.fabric.dispatcher().addMountItem("x");
        check(!tr.idle(-1) && tr.pending(-1).contains("rn_mounting:1") && !tr.unknown().contains("rn_loading"),
                "m3: trackReactNative() before start(context) tracks the app once it starts (" + tr.pending(-1) + ")");
    }

    static void optOutAndIgnoredOverTheWire() {
        OldArchApp app = new OldArchApp();
        Rn r = new Rn(true);
        app.host.getReactInstanceManager().context = r.ctx;
        Trackers tr = new Trackers();
        tr.setApplication(app);
        tr.enableFramework("react_native");
        SyncServer server = new SyncServer(new TrackerState(tr), 0, "com.example.app", 7);
        SyncServer.Session session = new SyncServer.Session();
        r.fabric.dispatcher().addMountItem("x");
        check(server.processCommand("{\"cmd\":\"status\"}", session).contains("\"idle\":false"), "M3: mounting busy");
        String cfg = server.processCommand("{\"cmd\":\"config\",\"ignore_trackers\":[\"rn_mounting\",\"nope\"]}", session);
        check(cfg.contains("\"unknown_trackers\":[\"nope\"]"), "M3: ignore_trackers reports names it doesn't know (" + cfg + ")");
        String st = server.processCommand("{\"cmd\":\"status\"}", session);
        check(st.contains("\"idle\":true"), "M3: an opted-out tracker never holds the app busy (" + st + ")");
        server.processCommand("{\"cmd\":\"config\",\"ignore_trackers\":[]}", session);
        check(server.processCommand("{\"cmd\":\"status\"}", session).contains("\"idle\":false"), "M3: an empty list opts back in");
        r.fabric.dispatcher().dispatchAll();
        // A perpetual animation: idle, and the reply says what was ignored.
        r.animated.getNodesManager().start(1, -1);
        // Drive the animation tracker past the bound on its own clock: use a short-clocked instance.
        Clock clock = new Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        Trackers tr2 = new Trackers();
        for (Tracker t : rn.trackers()) tr2.add(t);
        for (int i = 0; i < 160; i++) {
            clock.now.addAndGet(16);
            tr2.idle(-1);
        }
        SyncServer server2 = new SyncServer(new TrackerState(tr2), 0, "com.example.app", 7);
        String idle = server2.processCommand("{\"cmd\":\"status\"}", new SyncServer.Session());
        check(idle.contains("\"idle\":true") && idle.contains("ignored: perpetual rn_animation"),
                "M3: a perpetual animation is reported once in pending as ignored, idle (" + idle + ")");
    }

    // --- OkHttp ---------------------------------------------------------------

    static final class Chain implements Interceptor.Chain {
        final Request request;
        final NetworkTracker tracker;
        final IOException fail;
        int seenInFlight = -1;

        Chain(String url, NetworkTracker tracker, IOException fail) {
            this.request = new Request(url);
            this.tracker = tracker;
            this.fail = fail;
        }

        @Override
        public Request request() {
            return request;
        }

        @Override
        public Response proceed(Request r) throws IOException {
            seenInFlight = tracker.count();
            if (fail != null) throw fail;
            return new Response(200);
        }
    }

    static void okHttpInterceptor() throws Exception {
        NetworkTracker n = new NetworkTracker(new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS));
        AtalantaOkHttpInterceptor i = new AtalantaOkHttpInterceptor(n);
        Chain ok = new Chain("https://api.example.com/items", n, null);
        Response r = i.intercept(ok);
        check(r.code == 200 && ok.seenInFlight == 1 && n.count() == 0,
                "P3: AtalantaOkHttpInterceptor counts a request while it is in flight");
        Chain bad = new Chain("https://api.example.com/items", n, new IOException("reset"));
        boolean threw = false;
        try {
            i.intercept(bad);
        } catch (IOException e) {
            threw = true;
        }
        check(threw && bad.seenInFlight == 1 && n.count() == 0, "P3: a failed request is finished too, and the error propagates");
        Chain ws = new Chain("wss://chat.example.com/live", n, null);
        i.intercept(ws);
        check(ws.seenInFlight == 0, "P3: a WebSocket URL never counts");
        AtomicInteger concurrent = new AtomicInteger();
        List<Thread> threads = new ArrayList<>();
        for (int k = 0; k < 8; k++) {
            Thread t = new Thread(() -> {
                try {
                    i.intercept(new Chain("https://a/" + concurrent.incrementAndGet(), n, null));
                } catch (IOException e) {
                    // not thrown
                }
            });
            threads.add(t);
            t.start();
        }
        for (Thread t : threads) t.join();
        check(n.count() == 0, "P3: concurrent requests all finish to 0");
    }

    static void okHttpBody() throws Exception {
        NetworkTracker n = new NetworkTracker(new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS));
        AtalantaOkHttpInterceptor i = new AtalantaOkHttpInterceptor(n);
        // Headers arrived; the body is read afterwards (Retrofit/enqueue).
        Response r = i.intercept(new BodyChain("https://api.example.com/items", "{\"items\":[1,2,3]}"));
        check(n.count() == 1, "M4: still counted after the headers arrived (the body is unread)");
        okio.BufferedSource src = r.body().source();
        okio.Buffer sink = new okio.Buffer();
        src.read(sink, 4);
        check(n.count() == 1, "M4: ... and while the body is being read");
        String rest = src.readUtf8();
        check(n.count() == 0 && rest.endsWith("]}"), "M4: done at the end of the body");
        r.close();
        src.close();
        check(n.count() == 0, "M4: exactly once (EOF, then close twice)");
        // Closed without reading (Response.close()).
        Response r2 = i.intercept(new BodyChain("https://api.example.com/items", "abc"));
        check(n.count() == 1, "M4: counted");
        r2.close();
        check(n.count() == 0, "M4: Response.close() without reading ends it");
        r2.close();
        check(n.count() == 0, "M4: ... once");
        // A read that fails.
        Response r3 = i.intercept(new BodyChain("https://api.example.com/items", null));
        boolean threw = false;
        try {
            r3.body().source().read(new okio.Buffer(), 10);
        } catch (IOException e) {
            threw = true;
        }
        check(threw && n.count() == 0, "M4: a body read error ends it, and the error propagates");
        // No body.
        i.intercept(new Chain("https://api.example.com/none", n, null));
        check(n.count() == 0, "M4: a response without a body ends at once");
        // A blocklisted URL: not counted, body untouched.
        Response ws = i.intercept(new BodyChain("wss://chat.example.com/live", "x"));
        check(n.count() == 0 && !(ws.body() instanceof AtalantaOkHttpInterceptor.CountedBody), "M4: a WebSocket URL never counts");
        okHttpLeakedBody();
    }

    static void okHttpLeakedBody() throws Exception {
        Clock clock = new Clock();
        NetworkTracker n = new NetworkTracker(new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS), clock);
        AtalantaOkHttpInterceptor i = new AtalantaOkHttpInterceptor(n);
        Response leaked = i.intercept(new BodyChain("https://api.example.com/leak", "never read"));
        clock.now.addAndGet(NetworkTracker.BODY_MAX_MS - 1);
        check(n.count() == 1, "fi4-m6: a body not finished yet counts until the bound");
        clock.now.addAndGet(2);
        check(n.count() == 0 && ProbeLog.warned("network-body-leak"),
                "fi4-m6: a body never read nor closed stops counting after " + (NetworkTracker.BODY_MAX_MS / 1000) + " s, with a warning");
        Response next = i.intercept(new BodyChain("https://api.example.com/next", "x"));
        check(n.count() == 1, "fi4-m6: ... and a later request still counts");
        leaked.close();
        check(n.count() == 1, "fi4-m6: closing the leaked body later doesn't end the other one");
        next.close();
        check(n.count() == 0, "fi4-m6: exactly once each");
        // fy2-m6: a slow download still being read (a chunk every 10 s for 60 s) keeps counting.
        StringBuilder big = new StringBuilder();
        for (int k = 0; k < 10; k++) big.append("chunk").append(k);
        Response slow = i.intercept(new BodyChain("https://api.example.com/export", big.toString()));
        okio.BufferedSource src = slow.body().source();
        boolean counted = true;
        for (int k = 0; k < 6; k++) {
            clock.now.addAndGet(10_000);
            src.read(new okio.Buffer(), 6);
            counted &= n.count() == 1;
        }
        clock.now.addAndGet(NetworkTracker.BODY_MAX_MS - 1);
        boolean stillAfterLastRead = n.count() == 1;
        clock.now.addAndGet(2);
        check(counted && stillAfterLastRead && n.count() == 0,
                "fy2-m6: a body still being read past " + (NetworkTracker.BODY_MAX_MS / 1000) + " s keeps counting;"
                        + " the bound runs from its last read (counted " + counted + ", " + stillAfterLastRead + ")");
        slow.close();
    }

    /** A chain whose response has a body (null text: a body whose reads fail). */
    static final class BodyChain implements Interceptor.Chain {
        final Request request;
        final String text;

        BodyChain(String url, String text) {
            this.request = new Request(url);
            this.text = text;
        }

        @Override
        public Request request() {
            return request;
        }

        @Override
        public Response proceed(Request r) {
            if (text != null) {
                return new Response(200, okhttp3.ResponseBody.create(null, text.getBytes(java.nio.charset.StandardCharsets.UTF_8)));
            }
            okio.Source failing = new okio.Source() {
                public long read(okio.Buffer sink, long byteCount) throws IOException { throw new IOException("reset"); }
                public okio.Timeout timeout() { return okio.Timeout.NONE; }
                public void close() {}
            };
            final okio.BufferedSource s = okio.Okio.buffer(failing);
            return new Response(200, new okhttp3.ResponseBody() {
                @Override public okhttp3.MediaType contentType() { return null; }
                @Override public long contentLength() { return -1; }
                @Override public okio.BufferedSource source() { return s; }
            });
        }
    }
}
