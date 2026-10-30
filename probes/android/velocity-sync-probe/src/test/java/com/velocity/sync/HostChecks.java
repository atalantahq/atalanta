package com.velocity.sync;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.NetworkInterface;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Host JVM checks for the probe's Java logic (no device, no Gradle). Run by
 * host-check.sh with android.jar on the classpath for type resolution only.
 */
public final class HostChecks {
    private static int failures;

    static void check(boolean ok, String name) {
        System.out.println((ok ? "ok   " : "FAIL ") + name);
        if (!ok) failures++;
    }

    // Stand-ins for android.animation.ValueAnimator on the host.
    public static final class NewAnimator {
        static int running = 2;
        private static int getCurrentAnimationsCount() { return running; }
    }

    public static final class NoCounter {
        public static int getRunningAnimationCount() { return 7; } // the name the old probe used; not in AOSP
    }

    /**
     * The real counter is per-thread: ValueAnimator reads AnimationHandler's
     * ThreadLocal, so only the thread running the animators sees them.
     */
    public static final class PerThreadAnimator {
        static final ThreadLocal<int[]> HANDLER = new ThreadLocal<>();
        private static int getCurrentAnimationsCount() {
            int[] h = HANDLER.get();
            return h == null ? 0 : h[0];
        }
    }

    public static final class FallbackHandler {
        public static int getAnimationCount() { return 1; }
    }

    public static void main(String[] args) throws Exception {
        idleWait();
        blocklist();
        network();
        animations();
        animationsOnMain();
        mainThread();
        server();
        perConnection();
        hostileInput();
        stopWhileBinding();
        miniJsonFuzz();
        TrackerChecks.run();
        PollerChecks.run();
        RnHostChecks.run();
        System.out.println(failures == 0 ? "all host checks passed" : failures + " host check(s) FAILED");
        System.exit(failures == 0 ? 0 : 1);
    }

    static void idleWait() {
        long t0 = System.nanoTime();
        IdleWait.Outcome r = IdleWait.await(1000, -1, () -> true);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(r.idle && ms >= 30, "P5: steady idle answers after 3 samples, not the first (" + ms + " ms)");

        final long s = System.nanoTime();
        r = IdleWait.await(400, -1, () -> ((System.nanoTime() - s) / 1_000_000 / 25) % 2 == 0);
        check(!r.idle, "P5: idle in 25 ms gaps between work is not idle");

        final long s2 = System.nanoTime();
        r = IdleWait.await(1000, 50, () -> (System.nanoTime() - s2) / 1_000_000 > 100);
        check(r.idle && r.waitedMs >= 150, "P5: quiet_ms is honoured (" + r.waitedMs + " ms)");

        r = IdleWait.await(0, -1, () -> true);
        check(!r.idle, "P5: zero timeout does not claim idle");
    }

    static void blocklist() {
        UrlBlocklist b = new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS);
        check(b.isIgnored("wss://chat.example.com/live"), "P6: wss is ignored");
        check(b.isIgnored("https://x.example.com/socket.io/?EIO=4&transport=polling"), "P6: socket.io long-poll is ignored");
        check(b.isIgnored("https://x.example.com/api/long-poll?since=3"), "P6: long-poll is ignored");
        check(!b.isIgnored("https://x.example.com/api/items"), "P6: an ordinary request counts");
        check(b.add(Arrays.asList("/analytics/", "(")).equals(Collections.singletonList("(")), "P6: invalid patterns reported");
        check(b.isIgnored("https://x.example.com/analytics/beacon"), "P6: added pattern applies");
        b.set(Collections.<String>emptyList());
        check(!b.isIgnored("wss://chat.example.com/live"), "P6: replace drops the defaults");
    }

    static void network() {
        NetworkTracker n = new NetworkTracker(new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS));
        boolean a = n.started("https://api.example.com/items");
        boolean ws = n.started("wss://api.example.com/socket");
        check(a && !ws && n.count() == 1, "P4c/P6: a request counts, a WebSocket does not");
        n.finished(ws);
        check(n.count() == 1, "P4c: finishing an untracked request changes nothing");
        n.finished(a);
        check(n.count() == 0, "P4c: finished request is idle");
        n.decrement();
        n.increment();
        check(n.count() == 1, "P4c: an unmatched decrement can't hide a later request");
    }

    static void animations() {
        Object[][] real = {{NewAnimator.class, "getCurrentAnimationsCount"}};
        AnimationCounter c = new AnimationCounter(real, () -> Boolean.TRUE);
        check(c.available() && c.count() == 2, "P4a: getCurrentAnimationsCount is read (private, static)");
        NewAnimator.running = 0;
        check(c.count() == 0, "P4a: zero running animators is idle");

        Object[][] old = {{NoCounter.class, "getCurrentAnimationsCount"}};
        check(new AnimationCounter(old, () -> Boolean.TRUE).count() == AnimationCounter.UNKNOWN,
                "P4a: no counter + animators on is UNKNOWN, not idle (old probe: idle)");
        check(new AnimationCounter(old, () -> null).count() == AnimationCounter.UNKNOWN,
                "P4a: no counter + scale unknown is UNKNOWN");
        check(new AnimationCounter(old, () -> Boolean.FALSE).count() == 0,
                "P4a: no counter but animators disabled (scale 0) is idle");

        Object[][] fallback = {{"no.such.Class", "getCurrentAnimationsCount"},
                {NoCounter.class, "getCurrentAnimationsCount"},
                {FallbackHandler.class, "getAnimationCount"}};
        check(new AnimationCounter(fallback, () -> Boolean.TRUE).count() == 1,
                "P4a: falls back to AnimationHandler.getAnimationCount");

        // android.jar's public stubs carry no hidden methods: the framework
        // counter is unavailable on the host, so it must answer UNKNOWN.
        AnimationCounter fw = AnimationCounter.forFramework(null);
        check(!fw.available() && fw.count() == AnimationCounter.UNKNOWN,
                "P4a: framework counter unreadable -> UNKNOWN");
    }

    /** MAJOR-1: the animator count is read on the main thread, not the socket thread. */
    static void animationsOnMain() throws Exception {
        ExecutorService main = Executors.newSingleThreadExecutor();
        main.submit(() -> PerThreadAnimator.HANDLER.set(new int[] {3})).get();
        MainThreadCheck.Poster poster = r -> {
            main.execute(r);
            return true;
        };
        Object[][] perThread = {{PerThreadAnimator.class, "getCurrentAnimationsCount"}};

        AnimationCounter offMain = new AnimationCounter(perThread, () -> Boolean.TRUE);
        check(offMain.count() == 0,
                "MAJOR-1: read on the socket thread, a per-thread counter is 0 (the old probe's read)");
        AnimationCounter onMain = new AnimationCounter(perThread, () -> Boolean.TRUE, poster);
        check(onMain.count() == 3, "MAJOR-1: read on the main thread it sees the running animators");
        main.submit(() -> PerThreadAnimator.HANDLER.get()[0] = 0).get();
        check(onMain.count() == 0, "MAJOR-1: and 0 once they ended");

        main.execute(() -> sleep(300));
        sleep(20);
        long t0 = System.nanoTime();
        int busy = onMain.count();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(busy == AnimationCounter.MAIN_THREAD_BUSY && ms < 100,
                "MAJOR-1: a main thread that can't answer in time reads busy, never 0 (" + ms + " ms)");
        sleep(400);

        AnimationCounter refused = new AnimationCounter(perThread, () -> Boolean.TRUE, r -> false);
        check(refused.count() == AnimationCounter.MAIN_THREAD_BUSY, "MAJOR-1: a looper that refuses posts is not idle");

        Object[][] none = {{NoCounter.class, "getCurrentAnimationsCount"}};
        check(new AnimationCounter(none, () -> Boolean.TRUE, poster).count() == AnimationCounter.UNKNOWN,
                "MAJOR-1: no counter is still UNKNOWN with a main-thread read");

        // Inline poster (already on the main thread) must not deadlock.
        AnimationCounter inline = new AnimationCounter(perThread, () -> Boolean.TRUE, r -> {
            r.run();
            return true;
        });
        check(inline.count() == 0, "MAJOR-1: an inline poster answers without waiting");
        main.shutdownNow();
    }

    static void mainThread() throws Exception {
        ExecutorService main = Executors.newSingleThreadExecutor();
        AtomicBoolean queueIdle = new AtomicBoolean(true);
        MainThreadCheck check = new MainThreadCheck(r -> {
            main.execute(r);
            return true;
        }, queueIdle::get);
        check(check.isIdle(), "P4b: idle main thread answers idle");

        // An 800 ms click handler with nothing queued behind it: the old
        // MessageQueue.isIdle read idle here.
        main.execute(() -> sleep(300));
        sleep(20);
        long t0 = System.nanoTime();
        boolean idle = check.isIdle();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(!idle && ms < 100, "P4b: main thread executing a long message is busy (" + ms + " ms)");
        java.util.concurrent.atomic.AtomicInteger posts = new java.util.concurrent.atomic.AtomicInteger();
        MainThreadCheck counted = new MainThreadCheck(r -> {
            posts.incrementAndGet();
            main.execute(r);
            return true;
        }, queueIdle::get);
        for (int k = 0; k < 5; k++) counted.isIdle();
        check(posts.get() == 1, "n4: a blocked main thread gets one sentinel, not one per sample (" + posts.get() + ")");
        sleep(400);
        check(check.isIdle(), "P4b: idle again after the message");

        queueIdle.set(false);
        check(!check.isIdle(), "P4b: messages due on the queue are busy");
        queueIdle.set(true);

        MainThreadCheck gone = new MainThreadCheck(r -> false, () -> true);
        check(!gone.isIdle(), "P4b: a looper that refuses posts is not idle");
        main.shutdownNow();
    }

    static final class FakeState implements ProbeState {
        volatile boolean idle = true;
        public boolean idleNow() { return idle; }
        public List<String> pendingNow() { return idle ? new ArrayList<>() : Collections.singletonList("network:1"); }
        public List<String> unknownTrackers() { return Collections.singletonList("animation"); }
    }

    /** A 1 s timer is pending: busy unless the connection's horizon is shorter. */
    static final class TimerState implements ProbeState {
        public boolean idleNow() { return idleNow(-1); }
        public List<String> pendingNow() { return pendingNow(-1); }
        @Override public boolean idleNow(long maxTimerMs) { return maxTimerMs >= 0 && maxTimerMs < 1000; }
        @Override public List<String> pendingNow(long maxTimerMs) {
            return idleNow(maxTimerMs) ? new ArrayList<>() : Collections.singletonList("timer:1");
        }
        public List<String> unknownTrackers() { return new ArrayList<>(); }
    }

    /** Send {@code lines} on one connection; the replies, in order. */
    static List<String> askAll(int port, String... lines) {
        List<String> out = new ArrayList<>();
        try (Socket s = new Socket()) {
            s.connect(new InetSocketAddress("127.0.0.1", port), 1000);
            s.setSoTimeout(5000);
            OutputStream o = s.getOutputStream();
            BufferedReader in = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8));
            for (String line : lines) {
                o.write((line + "\n").getBytes(StandardCharsets.UTF_8));
                o.flush();
                out.add(in.readLine());
            }
        } catch (Exception e) {
            // the replies so far
        }
        return out;
    }

    /** probe-urls: max_timer_ms is per connection; ignore_urls are not added twice. */
    static void perConnection() throws Exception {
        SyncServer server = new SyncServer(new TimerState(), 0, "com.example.app", 7);
        server.start();
        check(server.awaitBound(2000), "probe-urls: server binds");
        int port = server.localPort();
        List<String> short_ = askAll(port, "{\"cmd\":\"config\",\"max_timer_ms\":100}",
                "{\"cmd\":\"wait_idle\",\"timeout_ms\":300}");
        check(short_.size() == 2 && short_.get(1) != null && short_.get(1).contains("\"idle\":true"),
                "probe-urls: max_timer_ms reaches the idle check on that connection (" + short_ + ")");
        String other = ask("127.0.0.1", port, "{\"cmd\":\"status\"}");
        check(other != null && other.contains("timer:1"),
                "probe-urls: another connection keeps the probe's own horizon (" + other + ")");
        server.stop();

        UrlBlocklist list = new UrlBlocklist(UrlBlocklist.DEFAULT_PATTERNS);
        int before = list.size();
        list.add(Arrays.asList("/beacon/", "/beacon/"));
        list.add(Arrays.asList("/beacon/"));
        check(list.size() == before + 1, "probe-urls: a pattern already on the list is not added again");
    }

    static String ask(String host, int port, String line) {
        try (Socket s = new Socket()) {
            s.connect(new InetSocketAddress(host, port), 1000);
            s.setSoTimeout(5000);
            OutputStream out = s.getOutputStream();
            out.write((line + "\n").getBytes(StandardCharsets.UTF_8));
            out.flush();
            return new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8)).readLine();
        } catch (Exception e) {
            return null;
        }
    }

    static void server() throws Exception {
        Map<String, String> env = new HashMap<>();
        check(SyncServer.resolvePort(env) == 19401, "P2: default port 19401");
        env.put("VELOCITY_SYNC_PORT", "41000");
        check(SyncServer.resolvePort(env) == 41000, "P2: VELOCITY_SYNC_PORT");
        env.put("VELOCITY_SYNC_PORT", "nope");
        check(SyncServer.resolvePort(env) == 19401, "P2: bad VELOCITY_SYNC_PORT falls back");

        FakeState state = new FakeState();
        SyncServer server = new SyncServer(state, 0, "com.example.app", 4242);
        server.start();
        check(server.awaitBound(2000), "server binds");
        int port = server.localPort();

        String hello = ask("127.0.0.1", port, "{\"cmd\":\"hello\"}");
        check(hello != null && hello.contains("\"bundle\":\"com.example.app\"") && hello.contains("\"pid\":4242")
                && hello.contains("\"version\":4") && hello.contains("\"unknown\":[\"animation\"]"),
                "P2: hello returns package + pid + unknown trackers (" + hello + ")");

        SyncServer multi = new SyncServer(state, 0, "com.example.app", "com.example.app:ui", 4243);
        multi.start();
        check(multi.awaitBound(2000), "m7: second server binds");
        String named = ask("127.0.0.1", multi.localPort(), "{\"cmd\":\"hello\"}");
        check(named != null && named.contains("\"bundle\":\"com.example.app\"")
                && named.contains("\"package\":\"com.example.app\"")
                && named.contains("\"process\":\"com.example.app:ui\""),
                "m7: hello names the package, the process separately (" + named + ")");
        multi.stop();

        String idle = ask("127.0.0.1", port, "{\"cmd\":\"wait_idle\",\"timeout_ms\":500,\"quiet_ms\":20}");
        check(idle != null && idle.contains("\"idle\":true"), "P5: wait_idle over the wire (" + idle + ")");

        state.idle = false;
        String busy = ask("127.0.0.1", port, "{\"cmd\":\"wait_idle\",\"timeout_ms\":100}");
        check(busy != null && busy.contains("\"idle\":false") && busy.contains("network:1"), "busy reply names pending (" + busy + ")");
        state.idle = true;

        String cfg = ask("127.0.0.1", port, "{\"cmd\":\"config\",\"ignore_urls\":[\"/beacon/\",\"(\"]}");
        check(cfg != null && cfg.contains("\"ok\":true") && cfg.contains("\"invalid\":[\"(\"]")
                && UrlBlocklist.SHARED.isIgnored("https://a/beacon/1"), "P6: config adds ignore_urls (" + cfg + ")");

        String unknown = ask("127.0.0.1", port, "{\"cmd\":\"bogus\"}");
        check(unknown != null && unknown.contains("unknown command"), "unknown command is an error reply");
        String bad = ask("127.0.0.1", port, "not json");
        check(bad != null && bad.contains("invalid command"), "bad JSON is an error reply");
        String escaped = ask("127.0.0.1", port, "{\"cmd\":\"x\\\"y\"}");
        check(escaped != null && escaped.contains("x\\\"y"), "error replies are valid JSON (" + escaped + ")");

        String lan = lanAddress();
        if (lan != null) {
            check(ask(lan, port, "{\"cmd\":\"hello\"}") == null, "P2: listens on loopback only (not " + lan + ")");
        } else {
            System.out.println("skip loopback-only check: no non-loopback IPv4 address");
        }
        server.stop();
    }

    /** m8: deeply nested or huge lines are error replies or a closed connection, never an Error. */
    static void hostileInput() throws Exception {
        StringBuilder deep = new StringBuilder("{\"cmd\":\"x\",\"a\":");
        for (int k = 0; k < 20_000; k++) deep.append('[');
        String nested = deep.toString();
        boolean parsed;
        try {
            MiniJson.parseObject(nested);
            parsed = true;
        } catch (IllegalArgumentException e) {
            parsed = false;
        } catch (StackOverflowError e) {
            parsed = true; // the old parser
        }
        check(!parsed, "m8: 20k nested '[' is rejected, not a StackOverflowError");
        StringBuilder ok = new StringBuilder("{\"cmd\":\"status\",\"a\":");
        for (int k = 0; k < 30; k++) ok.append('[');
        for (int k = 0; k < 30; k++) ok.append(']');
        check(MiniJson.parseObject(ok.append('}').toString()).get("cmd").equals("status"),
                "m8: ordinary nesting still parses");

        FakeState state = new FakeState();
        SyncServer server = new SyncServer(state, 0, "com.example.app", 1);
        server.start();
        check(server.awaitBound(2000), "m8: server binds");
        int port = server.localPort();
        String reply = ask("127.0.0.1", port, nested);
        check(reply != null && reply.contains("invalid command"), "m8: nested line gets an error reply (" + reply + ")");
        char[] big = new char[SyncServer.MAX_LINE_CHARS + 10];
        Arrays.fill(big, ' ');
        check(ask("127.0.0.1", port, "{" + new String(big) + "}") == null, "m8: an over-long line closes the connection");
        String after = ask("127.0.0.1", port, "{\"cmd\":\"status\"}");
        check(after != null && after.contains("\"idle\":true"), "m8: the server still answers afterwards");
        server.stop();
    }

    /** m9: stop() racing the bind thread must not leave the port listening. */
    static void stopWhileBinding() throws Exception {
        int port;
        try (java.net.ServerSocket probe = new java.net.ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))) {
            port = probe.getLocalPort();
        }
        SyncServer server = new SyncServer(new FakeState(), port, "com.example.app", 1);
        server.start();
        server.stop(); // before the bind thread has bound
        sleep(300);
        check(!listening(port), "m9: stop() during bind leaves nothing listening");

        server.start();
        server.stop();
        server.start(); // restarted while the first bind may still be running
        check(server.awaitBound(2000), "m9: restart binds");
        sleep(100);
        String hello = ask("127.0.0.1", port, "{\"cmd\":\"hello\"}");
        check(hello != null && hello.contains("\"hello\":true"), "m9: restarted server answers (" + hello + ")");
        server.stop();
        sleep(100);
        check(!listening(port), "m9: stopped server releases the port");

        // C7: the restart raced the previous start's bind thread for the
        // port and lost about half the time ("Address already in use").
        // Fifty stop/start cycles in a row must all bind.
        int boundCycles = 0;
        for (int i = 0; i < 50; i++) {
            server.start();
            server.stop();
            server.start();
            if (server.awaitBound(2000)) boundCycles++;
            server.stop();
        }
        check(boundCycles == 50, "m9: every quick restart binds (" + boundCycles + "/50)");
        sleep(100);
        check(!listening(port), "m9: the port is free after the restarts");
    }

    /**
     * C8: property checks on MiniJson with random input (fixed seed, so a
     * failure reproduces). Anything a host connection sends must either
     * parse or be refused with IllegalArgumentException: any other
     * exception (or error) would escape handleClient's contract. And what
     * write() produces must parse back to the same values.
     */
    static void miniJsonFuzz() {
        java.util.Random rnd = new java.util.Random(0xC8L);
        String alphabet = "{}[]\":,\\ unlt0123456789.eE+-truefalsenull\u00e9\u0000\n";
        String[] seeds = {
            "{\"cmd\":\"hello\"}",
            "{\"cmd\":\"config\",\"ignore_urls\":[\"a.*\",\"b\"],\"replace\":false,\"max_timer_ms\":1500}",
            "{\"cmd\":\"wait_idle\",\"timeout_ms\":5000,\"quiet_ms\":50,\"x\":{\"y\":[1,2.5e3,null,true]}}",
            "{\"s\":\"\\u00e9\\n\\\"q\\\"\"}",
        };
        int unexpected = 0;
        String firstBad = null;
        for (int n = 0; n < 3000; n++) {
            String input;
            if (n % 2 == 0) {
                // A valid message with a few random edits.
                StringBuilder b = new StringBuilder(seeds[rnd.nextInt(seeds.length)]);
                int edits = 1 + rnd.nextInt(4);
                for (int e = 0; e < edits && b.length() > 0; e++) {
                    int at = rnd.nextInt(b.length());
                    switch (rnd.nextInt(3)) {
                        case 0: b.deleteCharAt(at); break;
                        case 1: b.insert(at, alphabet.charAt(rnd.nextInt(alphabet.length()))); break;
                        default: b.setLength(at); break;
                    }
                }
                input = b.toString();
            } else {
                // Random characters from JSON's alphabet.
                StringBuilder b = new StringBuilder();
                int len = rnd.nextInt(40);
                for (int k = 0; k < len; k++) b.append(alphabet.charAt(rnd.nextInt(alphabet.length())));
                input = b.toString();
            }
            try {
                MiniJson.parseObject(input);
            } catch (IllegalArgumentException expected) {
                // refused: fine
            } catch (Throwable t) {
                unexpected++;
                if (firstBad == null) firstBad = input + " -> " + t;
            }
        }
        check(unexpected == 0, "C8: MiniJson refuses bad input only with IllegalArgumentException ("
                + unexpected + " other; first: " + firstBad + ")");

        int mismatches = 0;
        String firstMismatch = null;
        for (int n = 0; n < 500; n++) {
            Map<String, Object> m = new java.util.LinkedHashMap<>();
            int keys = rnd.nextInt(5);
            for (int k = 0; k < keys; k++) {
                StringBuilder v = new StringBuilder();
                int len = rnd.nextInt(12);
                for (int c = 0; c < len; c++) v.append((char) rnd.nextInt(0x250));
                m.put("k" + k, v.toString());
            }
            if (rnd.nextBoolean()) m.put("n", (long) rnd.nextInt());
            if (rnd.nextBoolean()) m.put("b", rnd.nextBoolean());
            if (rnd.nextBoolean()) m.put("z", null);
            String out = MiniJson.write(m);
            try {
                Map<String, Object> back = MiniJson.parseObject(out);
                if (!back.equals(m)) {
                    mismatches++;
                    if (firstMismatch == null) firstMismatch = out + " -> " + back;
                }
            } catch (Throwable t) {
                mismatches++;
                if (firstMismatch == null) firstMismatch = out + " -> " + t;
            }
        }
        check(mismatches == 0, "C8: MiniJson.write output parses back to the same values ("
                + mismatches + " differ; first: " + firstMismatch + ")");
    }

    static boolean listening(int port) {
        try (Socket s = new Socket()) {
            s.connect(new InetSocketAddress("127.0.0.1", port), 300);
            return true;
        } catch (Exception e) {
            return false;
        }
    }

    static String lanAddress() throws Exception {
        Enumeration<NetworkInterface> ifs = NetworkInterface.getNetworkInterfaces();
        while (ifs.hasMoreElements()) {
            NetworkInterface ni = ifs.nextElement();
            if (!ni.isUp() || ni.isLoopback()) continue;
            Enumeration<InetAddress> addrs = ni.getInetAddresses();
            while (addrs.hasMoreElements()) {
                InetAddress a = addrs.nextElement();
                if (a instanceof Inet4Address) return a.getHostAddress();
            }
        }
        return null;
    }

    static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }
}
