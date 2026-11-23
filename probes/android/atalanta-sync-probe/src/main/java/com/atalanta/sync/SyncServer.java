// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.Reader;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Exposes the app's idle state to the Atalanta runner. Listens on the
 * loopback interface only (the host reaches it through {@code adb forward
 * tcp:0 tcp:19401}, one host port per device); it used to listen on every
 * interface.
 *
 * <p>Protocol: newline-delimited JSON over TCP (same as the iOS probe).
 * <pre>
 *   {"cmd":"hello"}      -> {"hello":true,"bundle":"com.app","package":"com.app","process":"com.app:ui","pid":123,"version":3,"platform":"android","unknown":[]}
 *   {"cmd":"status"}     -> {"idle":true,"pending":[],"unknown":[],"ts":1710000000}
 *   {"cmd":"wait_idle","timeout_ms":5000,"quiet_ms":50}
 *                        -> {"idle":false,"pending":["network:1"],"unknown":[],"waited_ms":5000}
 *   {"cmd":"config","ignore_urls":["regex"],"replace":false,"max_timer_ms":1500} -> {"ok":true,"invalid":[]}
 *   {"cmd":"config","framework":"react_native"}
 *                        -> {"ok":true,"invalid":[],"framework":"react_native","unknown":[]}
 *   {"cmd":"config","ignore_trackers":["rn_animation"]}
 *                        -> {"ok":true,"invalid":[],"unknown_trackers":[]}
 * </pre>
 *
 * <p>{@code pending} also lists perpetual work the probe stopped waiting for
 * ({@code "ignored: perpetual rn_animation"}), in idle replies too.
 * {@code ignore_trackers} (process-wide, replacing the last list) names
 * trackers that never count: {@code timer}, {@code rn_loading},
 * {@code rn_timers}, {@code rn_mounting}, {@code rn_animation},
 * {@code rn_network}.
 *
 * <p>{@code ignore_urls} changes the process-wide list (a pattern already on
 * it is not added again); {@code max_timer_ms} holds for the connection that
 * sent it only, so two runs never see each other's. {@code framework} turns on
 * that framework's trackers for the process ({@code "framework":null} in the
 * reply: this probe has none for it).
 */
public final class SyncServer {
    /** Protocol version reported by {@code hello}; 1 was the protocol without it; 3 takes
     *  {@code max_timer_ms} in {@code config} for one connection only (sent to 3+ only);
     *  4 takes {@code framework} in {@code config} (React Native tracking) and tracks
     *  main-looper timers. */
    public static final int PROTOCOL_VERSION = 4;
    public static final int DEFAULT_PORT = 19401;
    private static final String TAG = "AtalantaSyncProbe";
    /** The longest request line; a longer one closes the connection. */
    static final int MAX_LINE_CHARS = 64 * 1024;

    private final ProbeState state;
    private final int port;
    private final String packageName;
    private final String processName;
    private final int pid;
    private volatile ServerSocket serverSocket;
    private volatile boolean running;
    private volatile CountDownLatch bound;
    /** Identifies the current start(); null once stopped. */
    private volatile Object generation;
    /**
     * Counted down once the latest start()'s thread holds no socket any
     * more (bind failed, it found itself stopped, or its accept loop
     * ended). The next start() waits for it before binding: a restart
     * right after stop() used to bind while the previous thread still
     * held (or was about to take) the port, and failed with "Address
     * already in use" (C7, host check m9).
     */
    private CountDownLatch released;
    /** How long a restart waits for the previous listener to let go. */
    static final long RELEASE_WAIT_MS = 5000;

    /** What one host connection set with {@code config}. */
    static final class Session {
        /** This connection's timer horizon; -1 is the probe's own. */
        volatile long maxTimerMs = -1;
    }

    public SyncServer(ProbeState state, int port, String packageName, int pid) {
        this(state, port, packageName, packageName, pid);
    }

    /** {@code processName} differs from the package under {@code android:process}. */
    public SyncServer(ProbeState state, int port, String packageName, String processName, int pid) {
        this.state = state;
        this.port = port;
        this.packageName = packageName;
        this.processName = processName;
        this.pid = pid;
    }

    /** {@code ATALANTA_SYNC_PORT} when set and valid, else 19401. */
    public static int resolvePort(Map<String, String> env) {
        String raw = env == null ? null : env.get("ATALANTA_SYNC_PORT");
        if (raw != null) {
            try {
                int p = Integer.parseInt(raw.trim());
                if (p > 0 && p < 65536) return p;
            } catch (NumberFormatException ignored) {
                // fall back
            }
        }
        return DEFAULT_PORT;
    }

    /** The bound port (useful when started on port 0), or -1. */
    public int localPort() {
        ServerSocket s = serverSocket;
        return s == null ? -1 : s.getLocalPort();
    }

    /**
     * Binds and accepts on a daemon thread: {@code start()} is called from
     * the main thread, where socket work trips StrictMode.
     */
    public synchronized void start() {
        if (running) return;
        running = true;
        final Object gen = new Object();
        generation = gen;
        final CountDownLatch done = new CountDownLatch(1);
        bound = done;
        final CountDownLatch previous = released;
        final CountDownLatch mine = new CountDownLatch(1);
        released = mine;
        Thread t = new Thread(() -> {
            ServerSocket s = null;
            try {
                // The previous listener closes its socket as soon as it
                // sees stop(); wait for that instead of racing it for
                // the port.
                if (previous != null) previous.await(RELEASE_WAIT_MS, TimeUnit.MILLISECONDS);
                if (generation != gen) {
                    // Stopped (or restarted again) while waiting.
                    return;
                }
                s = new ServerSocket();
                s.setReuseAddress(true);
                s.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), port), 50);
                synchronized (SyncServer.this) {
                    // stop() ran while this thread was binding: it had no
                    // socket to close, and the port used to stay listening
                    // with nothing accepting.
                    if (generation != gen) {
                        closeQuietly(s);
                        s = null;
                    } else {
                        serverSocket = s;
                    }
                }
                if (s != null) log("Sync server listening on 127.0.0.1:" + s.getLocalPort());
            } catch (Exception e) {
                closeQuietly(s);
                s = null;
                synchronized (SyncServer.this) {
                    if (generation == gen) running = false;
                }
                log("Server failed to start on port " + port + ": " + e.getMessage());
            } finally {
                done.countDown();
                if (s == null) mine.countDown();
            }
            if (s != null) {
                try {
                    acceptLoop(s, gen);
                } finally {
                    mine.countDown();
                }
            }
        }, "atalanta-sync-server");
        t.setDaemon(true);
        t.start();
    }

    /** Wait until the listener is bound (or failed); true when listening. */
    public boolean awaitBound(long timeoutMs) throws InterruptedException {
        CountDownLatch b = bound;
        return b != null && b.await(timeoutMs, TimeUnit.MILLISECONDS) && serverSocket != null;
    }

    public synchronized void stop() {
        running = false;
        generation = null;
        ServerSocket s = serverSocket;
        serverSocket = null;
        closeQuietly(s);
    }

    private static void closeQuietly(ServerSocket s) {
        if (s == null) return;
        try {
            s.close();
        } catch (Exception ignored) {
            // closing
        }
    }

    private void acceptLoop(ServerSocket s, Object gen) {
        try {
            while (running && generation == gen) {
                try {
                    final Socket client = s.accept();
                    Thread t = new Thread(() -> handleClient(client), "atalanta-sync-client");
                    t.setDaemon(true);
                    t.start();
                } catch (Exception e) {
                    if (running && generation == gen) log("Accept error: " + e.getMessage());
                }
            }
        } finally {
            closeQuietly(s);
        }
    }

    private void handleClient(Socket socket) {
        try (Socket sock = socket) {
            BufferedReader reader = new BufferedReader(
                    new InputStreamReader(sock.getInputStream(), StandardCharsets.UTF_8));
            OutputStream out = sock.getOutputStream();
            Session session = new Session();
            String line;
            while (running && (line = readLine(reader, MAX_LINE_CHARS)) != null) {
                if (line.trim().isEmpty()) continue;
                out.write((processCommand(line, session) + "\n").getBytes(StandardCharsets.UTF_8));
                out.flush();
            }
        } catch (Throwable t) {
            // Client went away, or sent something hostile. Any app (or adb
            // forward) on the device can reach loopback: nothing here may
            // reach the app's uncaught-exception handler.
        }
    }

    /** One line without its terminator, null at end of stream; too long is an IOException. */
    static String readLine(Reader r, int max) throws IOException {
        StringBuilder b = new StringBuilder();
        int c;
        while ((c = r.read()) != -1) {
            if (c == '\n') break;
            if (b.length() >= max) throw new IOException("request line too long");
            b.append((char) c);
        }
        if (c == -1 && b.length() == 0) return null;
        int n = b.length();
        if (n > 0 && b.charAt(n - 1) == '\r') b.setLength(n - 1);
        return b.toString();
    }

    /** One request line in, one reply line out (a one-line connection). */
    String processCommand(String json) {
        return processCommand(json, new Session());
    }

    /** One request line in, one reply line out, on {@code session}'s connection. */
    String processCommand(String json, Session session) {
        Map<String, Object> cmd;
        try {
            cmd = MiniJson.parseObject(json);
        } catch (IllegalArgumentException e) {
            return error("invalid command");
        }
        Object name = cmd.get("cmd");
        if (!(name instanceof String)) return error("invalid command");
        switch ((String) name) {
            case "hello":
                return hello();
            case "status":
                return status(session.maxTimerMs);
            case "wait_idle":
                return waitIdle(asLong(cmd.get("timeout_ms"), 5000), asLong(cmd.get("quiet_ms"), -1),
                        session.maxTimerMs);
            case "config":
                return config(cmd, session);
            default:
                return error("unknown command: " + name);
        }
    }

    private static long asLong(Object v, long dflt) {
        return v instanceof Number ? ((Number) v).longValue() : dflt;
    }

    private static String error(String msg) {
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("error", msg);
        return MiniJson.write(m);
    }

    private String hello() {
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("hello", Boolean.TRUE);
        m.put("bundle", packageName);
        m.put("package", packageName);
        m.put("process", processName);
        m.put("pid", pid);
        m.put("version", PROTOCOL_VERSION);
        m.put("platform", "android");
        m.put("unknown", state.unknownTrackers());
        return MiniJson.write(m);
    }

    private String status(long maxTimerMs) {
        Map<String, Object> m = new LinkedHashMap<>();
        boolean idle = state.idleNow(maxTimerMs);
        m.put("idle", idle);
        m.put("pending", idle ? ignored() : state.pendingNow(maxTimerMs));
        m.put("unknown", state.unknownTrackers());
        m.put("ts", System.currentTimeMillis() / 1000);
        return MiniJson.write(m);
    }

    private String waitIdle(long timeoutMs, long quietMs, long maxTimerMs) {
        IdleWait.Outcome o = IdleWait.await(timeoutMs, quietMs, () -> state.idleNow(maxTimerMs));
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("idle", o.idle);
        m.put("pending", o.idle ? ignored() : state.pendingNow(maxTimerMs));
        m.put("unknown", state.unknownTrackers());
        m.put("waited_ms", o.waitedMs);
        return MiniJson.write(m);
    }

    /** Perpetual work no longer waited for ({@code ignored: perpetual ...}); idle replies carry it too. */
    private List<String> ignored() {
        try {
            return new ArrayList<>(state.ignoredNow());
        } catch (Throwable t) {
            return new ArrayList<>();
        }
    }

    private String config(Map<String, Object> cmd, Session session) {
        List<String> invalid = new ArrayList<>();
        Object urls = cmd.get("ignore_urls");
        if (urls instanceof List) {
            List<String> patterns = new ArrayList<>();
            for (Object o : (List<?>) urls) {
                if (o instanceof String) patterns.add((String) o);
            }
            invalid = Boolean.TRUE.equals(cmd.get("replace"))
                    ? UrlBlocklist.SHARED.set(patterns)
                    : UrlBlocklist.SHARED.add(patterns);
        }
        Object maxTimer = cmd.get("max_timer_ms");
        if (maxTimer instanceof Number) {
            session.maxTimerMs = Math.max(0, ((Number) maxTimer).longValue());
        }
        Map<String, Object> m = new LinkedHashMap<>();
        m.put("ok", Boolean.TRUE);
        m.put("invalid", invalid);
        Object optOut = cmd.get("ignore_trackers");
        if (optOut instanceof List) {
            List<String> names = new ArrayList<>();
            for (Object o : (List<?>) optOut) {
                if (o instanceof String) names.add((String) o);
            }
            m.put("unknown_trackers", state.optOutTrackers(names));
        }
        Object framework = cmd.get("framework");
        if (framework instanceof String) {
            boolean on;
            try {
                on = state.enableFramework((String) framework);
            } catch (Throwable t) {
                on = false;
            }
            m.put("framework", on ? framework : null);
            // What the framework's trackers can't tell (sampled now).
            m.put("unknown", state.unknownTrackers());
        }
        return MiniJson.write(m);
    }

    private static void log(String msg) {
        try {
            android.util.Log.i(TAG, msg);
        } catch (Throwable t) {
            // host JVM (android.jar stubs throw)
            System.err.println(TAG + ": " + msg);
        }
    }
}
