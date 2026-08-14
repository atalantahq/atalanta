package com.velocity.tree;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.UiAutomation;
import android.graphics.Bitmap;
import android.graphics.Rect;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import java.io.ByteArrayOutputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Persistent UI-tree server for Velocity.
 *
 * Runs on the device as the shell user via app_process (the same way
 * `uiautomator` itself runs), holds one UiAutomation connection, and serves
 * the active window's accessibility tree over a local socket in the same XML
 * format `uiautomator dump` writes, so the host-side parser is unchanged.
 * The same connection injects input, takes screenshots and watches
 * accessibility events, so none of that needs an `adb shell` process.
 *
 * Protocol (one request per line, connection kept open):
 *   PING                     -> "PONG <version> <boot nonce>" (the nonce is
 *                               new each time the server starts)
 *   AUTH <token>             -> (protocol 13) "OK 0", or "ERR bad token" and
 *                               the connection is closed. Every command but
 *                               PING, AUTH and QUIT needs it first. The token
 *                               is new each start and written to TOKEN_FILE,
 *                               which only shell (or root) can read: the
 *                               host reads it with `adb shell cat`
 *   #<id> <command>          -> (protocol 8) the command, run at most once
 *                               per id: a command that changes the device
 *                               sent again with the same id (after a lost
 *                               reply and a reconnect) gets the first run's
 *                               reply, or waits for it, and never runs twice
 *   DUMP [lite|notag]        -> "OK <len>" + XML ("lite": only the attributes
 *                               a settle compares). Protocol 9 adds `hint`
 *                               (an empty field's label), `state-desc` and
 *                               `error` to full dumps; older hosts ignore
 *                               attributes they don't know. Protocol 10
 *                               adds `test-tag`: a Compose testTag read
 *                               through the accessibility extra data, for
 *                               apps without testTagsAsResourceId ("notag"
 *                               skips it; an older server reads "notag" as
 *                               a plain full dump). Bounded to 50 ms of
 *                               reads per dump, cached per node (TagReads).
 *                               Protocol 11 adds `role-desc`: the node's
 *                               role description (Compose's Role.Switch /
 *                               Role.Tab are a plain View with one), for
 *                               `role:` selectors
 *   TAP x y                  -> "OK 0"
 *   TAPS x y n               -> "OK 0"   (n taps, a double tap is n=2)
 *   PRESS x y ms             -> "OK 0"   (long press)
 *   SWIPE x1 y1 x2 y2 ms     -> "OK 0"
 *   KEY code                 -> "OK 0"
 *   TEXT <utf-8 hex>         -> "OK 0"   (typed as key events; characters
 *                               without a key are appended with SET_TEXT;
 *                               ERR when the input was refused)
 *   SCROLLNODE x y 1|-1 [v|h] -> "OK 1" scrolled, "OK 0" nothing to scroll
 *                               (the axis, protocol 8, skips lists that
 *                               scroll the other way)
 *   SETTEXT a|r <utf-8 hex>  -> "OK 1" applied, "OK 0" refused (the field is
 *                               unchanged), "OK 2 <hex read-back>" applied
 *                               but the field reads back differently (a mask
 *                               reformatted it): the text is in the field.
 *                               Protocol 9: "OK 0" for an append to a
 *                               non-empty password field (its text reads
 *                               back as bullets, which would be prepended);
 *                               the host types it as key events instead
 *   SHOT png|jpeg [quality]  -> "OK <len>" + image bytes
 *   WINDOWS                  -> "OK <len>" + one line per window, top first:
 *                               "<layer> <type> <l>,<t>,<r>,<b> <package>"
 *   MARK                     -> "OK <gen>"  (remember now as "the action")
 *   GEN                      -> "OK <gen> <ms since last event>"
 *   SETTLE reaction quiet cap
 *                            -> "OK <settled 0|1> <reacted 0|1> <ms>"
 *   ENCODERS                 -> (protocol 12) "OK <len>" + one line per
 *                               H.264 encoder: "<name> <hardware 0|1|?>".
 *                               The same jar also carries VideoServer, the
 *                               scrcpy-style screen video entry point
 *                               (started as its own app_process)
 *   QUIT                     -> server exits
 * Errors are "ERR <message>". Only adb (uid shell or root), never an app,
 * may connect: the socket name is public.
 *
 * Protocol 13 (R-2): the host side is an `adb forward` port on the Mac's
 * loopback, which a web page can reach with a blind `fetch()` POST. Its
 * request line (`POST / HTTP/1.1`) and headers used to be refused one by
 * one while the body's lines (`TAP 500 900`) ran. Now the first line that
 * is not a command, or a command before AUTH, gets one ERR and the
 * connection is closed, so nothing after it is read.
 */
public final class Server {
    static final String VERSION = "13";
    static final String SOCKET = "velocity_tree";
    /** New on every start: a host that reconnects can tell it is the same server. */
    static final String NONCE = Long.toHexString(new java.security.SecureRandom().nextLong());
    /** Protocol 13: the session token every connection presents with AUTH. */
    static final String TOKEN = newToken();
    /** Where the token is left for the host: owner-only, under adb's own directory. */
    static final String TOKEN_FILE = "/data/local/tmp/velocity-tree.token";
    /** Every request a connection may send; any other line closes it. */
    static final java.util.Set<String> COMMANDS = new java.util.HashSet<>(java.util.Arrays.asList(
            "PING", "AUTH", "DUMP", "TAP", "TAPS", "PRESS", "SWIPE", "KEY", "TEXT", "SETTEXT", "IME",
            "GLOBAL", "SCROLLNODE", "SHOT", "WINDOWS", "MARK", "GEN", "SETTLE", "ENCODERS", "QUIT"));
    /** What a connection may send before AUTH. */
    static final java.util.Set<String> BEFORE_AUTH = new java.util.HashSet<>(java.util.Arrays.asList(
            "PING", "AUTH", "QUIT"));
    /** The longest request line read; longer ones close the connection. */
    static final int MAX_LINE = 1 << 20;
    /** Node depth at which a dump stops descending (the Java stack). */
    static final int MAX_DEPTH = 200;

    private static UiAutomation automation;
    /** UiAutomation.clearCache() (API 34+), looked up once; null if absent. */
    private static java.lang.reflect.Method clearCache;
    private static final long CACHE_CLEAR_MS = 1000;
    private static long lastClear = 0;

    /** Accessibility events that mean the screen changed. */
    private static volatile long generation = 0;
    private static volatile long lastEventAt = 0;
    /** State at the last action (MARK or an injected input). */
    private static volatile long markGen = 0;
    private static volatile long markAt = 0;

    public static void main(String[] args) throws Exception {
        boolean keepA11y = java.util.Arrays.asList(args).contains("--keep-a11y");
        automation = connect(5000, keepA11y);
        try {
            clearCache = UiAutomation.class.getMethod("clearCache");
        } catch (NoSuchMethodException ignored) {
            clearCache = null;
        }
        // Needed for getWindows(); harmless where it's already set.
        try {
            AccessibilityServiceInfo info = automation.getServiceInfo();
            if (info != null) {
                info.flags |= AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS;
                automation.setServiceInfo(info);
            }
        } catch (Exception ignored) {
        }
        lastEventAt = SystemClock.uptimeMillis();
        automation.setOnAccessibilityEventListener(Server::onEvent);

        LocalServerSocket server = new LocalServerSocket(SOCKET);
        // After the bind: a second server that can't bind must not replace
        // the running one's token.
        writeToken();
        System.out.println("velocity-tree " + VERSION + " listening on @" + SOCKET);
        System.out.flush();
        while (true) {
            final LocalSocket client = server.accept();
            Thread t = new Thread(() -> serve(client));
            t.setDaemon(true);
            t.start();
        }
    }

    private static String newToken() {
        byte[] b = new byte[16];
        new java.security.SecureRandom().nextBytes(b);
        StringBuilder sb = new StringBuilder(32);
        for (byte x : b) sb.append(String.format("%02x", x & 0xff));
        return sb.toString();
    }

    /**
     * Leave TOKEN in TOKEN_FILE, readable by this uid (shell, or root) only:
     * created empty, restricted, filled, then renamed into place, so it is
     * never readable by anyone else. /data/local/tmp is shell's own
     * directory: apps can't plant anything there.
     */
    private static void writeToken() throws java.io.IOException {
        java.io.File tmp = new java.io.File(TOKEN_FILE + ".tmp");
        tmp.delete();
        try (java.io.FileOutputStream f = new java.io.FileOutputStream(tmp)) {
            boolean ok = tmp.setReadable(false, false) && tmp.setWritable(false, false)
                    && tmp.setReadable(true, true) && tmp.setWritable(true, true);
            if (!ok) throw new java.io.IOException("cannot restrict " + tmp);
            f.write((TOKEN + "\n").getBytes(StandardCharsets.UTF_8));
        }
        if (!tmp.renameTo(new java.io.File(TOKEN_FILE))) {
            throw new java.io.IOException("cannot write " + TOKEN_FILE);
        }
    }

    /**
     * Why a connection that sent `verb` must be closed, or null when the
     * line may run: anything but a command (an HTTP request line from a
     * browser's blind POST), or a command other than PING, AUTH and QUIT
     * before AUTH (R-2).
     */
    static String refusal(String verb, boolean authed) {
        if (!COMMANDS.contains(verb)) {
            String shown = verb.length() > 32 ? verb.substring(0, 32) : verb;
            return "unknown command " + shown.replaceAll("[^\\x21-\\x7e]", "?") + "; closing";
        }
        if (!authed && !BEFORE_AUTH.contains(verb)) return "AUTH required; closing";
        return null;
    }

    /** Whether `got` is the session token (constant time). */
    static boolean tokenMatches(String got) {
        return java.security.MessageDigest.isEqual(
                TOKEN.getBytes(StandardCharsets.UTF_8), got.getBytes(StandardCharsets.UTF_8));
    }

    /**
     * Connect through the device's own uiautomator wrapper (it knows how to
     * build a UiAutomation on this Android version), but with a deadline: a
     * connect that never returns used to hang startup until the host gave up.
     */
    private static UiAutomation connect(long timeoutMs, boolean keepA11y) throws Exception {
        Class<?> wrapperClass = Class.forName("com.android.uiautomator.core.UiAutomationShellWrapper");
        final Object wrapper = wrapperClass.getConstructor().newInstance();
        final Exception[] failure = new Exception[1];
        final UiAutomation[] own = new UiAutomation[1];
        Thread t = new Thread(() -> {
            try {
                // Default UiAutomation switches off every other accessibility
                // service (TalkBack, password managers, the app's own) while it
                // runs: deterministic, but an app can't be tested with them.
                // --keep-a11y connects with FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES
                // (API 24+; older devices ignore it).
                if (keepA11y) {
                    try {
                        wrapperClass.getMethod("connect", int.class).invoke(wrapper, 1);
                        return;
                    } catch (NoSuchMethodException ignored) {
                        // The shell wrapper has only connect() (API 33 and 34
                        // at least), which always suppresses: build the
                        // UiAutomation the way it does, then connect it with
                        // the flag ourselves.
                    }
                    own[0] = connectKeepingA11y();
                    if (own[0] != null) return;
                    System.err.println("velocity-tree: --keep-a11y unsupported here; accessibility services are paused");
                }
                wrapperClass.getMethod("connect").invoke(wrapper);
            } catch (Exception e) {
                failure[0] = e;
            }
        });
        t.setDaemon(true);
        t.start();
        t.join(timeoutMs);
        if (t.isAlive()) {
            throw new IllegalStateException("UiAutomation connect timed out after " + timeoutMs + " ms");
        }
        if (failure[0] != null) throw failure[0];
        if (own[0] != null) return own[0];
        return (UiAutomation) wrapperClass.getMethod("getUiAutomation").invoke(wrapper);
    }

    /**
     * A UiAutomation connected with FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES
     * (1), built as UiAutomationShellWrapper.connect() builds its own: a
     * handler thread's looper and a UiAutomationConnection. Null when this
     * Android version has no such constructor or connect(int).
     */
    private static UiAutomation connectKeepingA11y() throws Exception {
        Class<?> connIface = Class.forName("android.app.IUiAutomationConnection");
        Object connection = Class.forName("android.app.UiAutomationConnection")
                .getConstructor().newInstance();
        java.lang.reflect.Constructor<UiAutomation> ctor;
        try {
            ctor = UiAutomation.class.getConstructor(android.os.Looper.class, connIface);
        } catch (NoSuchMethodException e) {
            return null;
        }
        android.os.HandlerThread thread = new android.os.HandlerThread("VelocityUiAutomation");
        thread.start();
        UiAutomation ua = ctor.newInstance(thread.getLooper(), connection);
        try {
            UiAutomation.class.getMethod("connect", int.class).invoke(ua, 1);
        } catch (NoSuchMethodException e) {
            thread.quitSafely();
            return null;
        }
        return ua;
    }

    // The last toast: toasts are never windows a dump can read, but Android
    // announces each one as a notification event. Shown for up to a long
    // toast's duration so `assertVisible: "Saved!"` can see it.
    private static volatile String toastText;
    private static volatile long toastAt;
    private static final long TOAST_MS = 3500;

    private static void onEvent(AccessibilityEvent e) {
        if (e.getEventType() == AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED
                && e.getClassName() != null && e.getClassName().toString().contains("Toast")) {
            StringBuilder t = new StringBuilder();
            for (CharSequence c : e.getText()) { if (t.length() > 0) t.append(' '); t.append(c); }
            if (t.length() > 0) {
                toastText = t.toString();
                toastAt = SystemClock.uptimeMillis();
                generation++;
                lastEventAt = toastAt;
            }
            return;
        }
        switch (e.getEventType()) {
            case AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED:
            case AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED:
            case AccessibilityEvent.TYPE_WINDOWS_CHANGED:
            case AccessibilityEvent.TYPE_VIEW_SCROLLED:
            case AccessibilityEvent.TYPE_VIEW_TEXT_CHANGED:
            case AccessibilityEvent.TYPE_VIEW_SELECTED:
            case AccessibilityEvent.TYPE_VIEW_FOCUSED:
                generation++;
                lastEventAt = SystemClock.uptimeMillis();
                break;
            default:
                break;
        }
    }

    private static void mark() {
        markGen = generation;
        markAt = SystemClock.uptimeMillis();
    }

    private static void serve(LocalSocket client) {
        try (LocalSocket c = client) {
            OutputStream out = c.getOutputStream();
            // The abstract socket name is public: any app could connect and
            // drive the device as the shell user. adb's forward connects as
            // adbd (shell, or root); apps have uids from 10000 per user.
            int uid = c.getPeerCredentials().getUid();
            if (uid % 100000 >= 10000) {
                text(out, "ERR peer uid " + uid + " not allowed");
                return;
            }
            java.io.InputStream in = new java.io.BufferedInputStream(c.getInputStream());
            String line;
            boolean authed = false;
            while ((line = readLine(in)) != null) {
                String id = null;
                String body = line.trim();
                if (body.startsWith("#")) {
                    int sp = body.indexOf(' ');
                    id = sp < 0 ? body.substring(1) : body.substring(1, sp);
                    body = sp < 0 ? "" : body.substring(sp + 1).trim();
                }
                String[] p = body.split(" ");
                // R-2: one answer, then nothing more is read from this peer.
                String refused = refusal(p[0], authed);
                if (refused != null) {
                    text(out, "ERR " + refused);
                    out.flush();
                    return;
                }
                if (p[0].equals("AUTH")) {
                    authed = p.length == 2 && tokenMatches(p[1]);
                    text(out, authed ? "OK 0" : "ERR bad token; closing");
                    out.flush();
                    if (!authed) return;
                    continue;
                }
                if (p[0].equals("QUIT")) {
                    text(out, "BYE");
                    out.flush();
                    System.exit(0);
                }
                if (id != null && MUTATES.contains(p[0])) {
                    out.write(once(id, p));
                } else {
                    out.write(run(p));
                }
                out.flush();
            }
        } catch (Throwable ignored) {
            // Client went away (or an Error in this thread): only this
            // connection ends, never the server.
        }
    }

    /** One request line without its newline; null at EOF or past MAX_LINE. */
    private static String readLine(java.io.InputStream in) throws java.io.IOException {
        ByteArrayOutputStream b = new ByteArrayOutputStream(64);
        int c;
        while ((c = in.read()) != -1) {
            if (c == '\n') return new String(b.toByteArray(), StandardCharsets.UTF_8);
            if (b.size() >= MAX_LINE) return null;
            b.write(c);
        }
        return b.size() > 0 ? new String(b.toByteArray(), StandardCharsets.UTF_8) : null;
    }

    /** The reply to one command, an error included: never throws. */
    private static byte[] run(String[] p) {
        ByteArrayOutputStream buf = new ByteArrayOutputStream(64);
        try {
            handle(p, buf);
        } catch (Throwable e) {
            buf.reset();
            byte[] err = ("ERR " + oneLine(e) + "\n").getBytes(StandardCharsets.UTF_8);
            buf.write(err, 0, err.length);
        }
        return buf.toByteArray();
    }

    // ---- at most once (protocol 8) ----------------------------------------

    /** Commands that change the device: run at most once per request id. */
    static final java.util.Set<String> MUTATES = new java.util.HashSet<>(java.util.Arrays.asList(
            "TAP", "TAPS", "PRESS", "SWIPE", "KEY", "TEXT", "SETTEXT", "IME", "GLOBAL", "SCROLLNODE"));

    private static final class Done {
        final java.util.concurrent.CountDownLatch latch = new java.util.concurrent.CountDownLatch(1);
        volatile byte[] reply;
    }

    /** The last 64 request ids and their replies (or the run in progress). */
    private static final java.util.LinkedHashMap<String, Done> REPLIES =
            new java.util.LinkedHashMap<String, Done>(128, 0.75f, true) {
                @Override
                protected boolean removeEldestEntry(java.util.Map.Entry<String, Done> e) {
                    return size() > 64;
                }
            };

    /**
     * Run `p` once for `id`. The same id again (the host lost the reply,
     * reconnected and resent) gets the stored reply; while the first run is
     * still going it waits for it, so the two never both run.
     */
    private static byte[] once(String id, String[] p) throws InterruptedException {
        Done d;
        boolean first;
        synchronized (REPLIES) {
            d = REPLIES.get(id);
            first = d == null;
            if (first) {
                d = new Done();
                REPLIES.put(id, d);
            }
        }
        if (first) {
            try {
                d.reply = run(p);
            } finally {
                d.latch.countDown();
            }
            return d.reply;
        }
        if (!d.latch.await(60, java.util.concurrent.TimeUnit.SECONDS) || d.reply == null) {
            return ("ERR request " + id + " is still running\n").getBytes(StandardCharsets.UTF_8);
        }
        return d.reply;
    }

    private static void handle(String[] p, OutputStream out) throws Exception {
        switch (p[0]) {
            case "PING":
                text(out, "PONG " + VERSION + " " + NONCE);
                break;
            case "DUMP":
                payload(out, dump(p.length > 1 && p[1].equals("lite"),
                        !(p.length > 1 && p[1].equals("notag"))).getBytes(StandardCharsets.UTF_8));
                break;
            case "TAP":
                mark();
                taps(i(p[1]), i(p[2]), 1);
                text(out, "OK 0");
                break;
            case "TAPS":
                mark();
                taps(i(p[1]), i(p[2]), i(p[3]));
                text(out, "OK 0");
                break;
            case "PRESS":
                mark();
                swipe(i(p[1]), i(p[2]), i(p[1]), i(p[2]), i(p[3]));
                text(out, "OK 0");
                break;
            case "SWIPE":
                mark();
                swipe(i(p[1]), i(p[2]), i(p[3]), i(p[4]), i(p[5]));
                text(out, "OK 0");
                break;
            case "KEY":
                mark();
                key(i(p[1]));
                text(out, "OK 0");
                break;
            case "TEXT":
                mark();
                type(new String(unhex(p.length > 1 ? p[1] : ""), StandardCharsets.UTF_8));
                text(out, "OK 0");
                break;
            case "SETTEXT":
                // The focused field's whole text in one action: O(1) where
                // typing is a key event per character, and clearing one
                // DEL per character. Replies 1 when the field took it.
                mark();
                // SETTEXT a <hex> appends (inputText), SETTEXT r <hex> replaces (clear).
                text(out, "OK " + setText(p.length > 1 && p[1].equals("a"),
                        new String(unhex(p.length > 2 ? p[2] : ""), StandardCharsets.UTF_8)));
                break;
            case "IME":
                // The keyboard's own action (Search/Go/Send/Done) on the focused
                // field: a raw ENTER key misses it in many Compose/WebView fields.
                mark();
                text(out, "OK " + (imeEnter() ? 1 : 0));
                break;
            case "GLOBAL":
                // Back, home, recents, notifications, quick settings, shade:
                // no adb round-trip.
                mark();
                text(out, "OK " + (automation.performGlobalAction(i(p[1])) ? 1 : 0));
                break;
            case "SCROLLNODE":
                // Scroll the list under (x, y) by its own scroll action: no
                // fling to settle, and 0 means it can't move further that way.
                mark();
                // Protocol 8: "v" or "h" picks a list that scrolls that way,
                // not a carousel that happens to be under the point.
                text(out, "OK " + (scrollNode(i(p[1]), i(p[2]), i(p[3]) >= 0,
                        p.length > 4 ? p[4] : null) ? 1 : 0));
                break;
            case "SHOT":
                payload(out, screenshot(p.length > 1 ? p[1] : "png", p.length > 2 ? i(p[2]) : 80));
                break;
            case "WINDOWS":
                payload(out, windows().getBytes(StandardCharsets.UTF_8));
                break;
            case "MARK":
                mark();
                text(out, "OK " + markGen);
                break;
            case "GEN":
                text(out, "OK " + generation + " " + (SystemClock.uptimeMillis() - lastEventAt));
                break;
            case "SETTLE":
                text(out, "OK " + settle(i(p[1]), i(p[2]), i(p[3])));
                break;
            case "ENCODERS":
                payload(out, encoders().getBytes(StandardCharsets.UTF_8));
                break;
            case "QUIT":
                text(out, "BYE");
                out.flush();
                System.exit(0);
                break;
            default:
                text(out, "ERR unknown command " + p[0]);
        }
    }

    // ---- settle (A-4) ---------------------------------------------------

    /**
     * Since the last action: wait until the screen reacted (an event) or
     * `reaction` ms passed with none, then until no event for `quiet` ms;
     * give up at `cap` ms. Returns "settled reacted elapsed".
     */
    private static String settle(int reaction, int quiet, int cap) throws InterruptedException {
        long start = markAt > 0 ? markAt : SystemClock.uptimeMillis();
        long since = markGen;
        while (true) {
            long now = SystemClock.uptimeMillis();
            boolean changed = generation > since;
            boolean reacted = changed || now - start >= reaction;
            long quietFor = now - (changed ? lastEventAt : start);
            if (reacted && quietFor >= quiet) {
                return "1 " + (changed ? 1 : 0) + " " + (now - start);
            }
            if (now - start >= cap) {
                return "0 " + (changed ? 1 : 0) + " " + (now - start);
            }
            Thread.sleep(reacted ? Math.max(5, quiet - quietFor) : 10);
        }
    }

    // ---- input (A-6) ----------------------------------------------------

    /** injectInputEvent(event, sync, waitForAnimations), API 34+; null before. */
    private static java.lang.reflect.Method injectNoWait;
    private static boolean injectLookedUp = false;

    /**
     * Since API 34 the two-argument injectInputEvent also waits for running
     * animations to end (~20-30 ms a tap here). The settle that follows
     * does the waiting, so use the three-argument form without it.
     */
    private static boolean inject(android.view.InputEvent e, boolean sync) {
        if (!injectLookedUp) {
            injectLookedUp = true;
            try {
                injectNoWait = UiAutomation.class.getMethod(
                        "injectInputEvent", android.view.InputEvent.class, boolean.class, boolean.class);
            } catch (NoSuchMethodException ignored) {
                injectNoWait = null;
            }
        }
        if (injectNoWait != null) {
            try {
                return (Boolean) injectNoWait.invoke(automation, e, sync, false);
            } catch (Exception ignored) {
                // Hidden-API restrictions: fall back to the public form.
            }
        }
        return automation.injectInputEvent(e, sync);
    }

    private static MotionEvent touch(long down, int action, float x, float y) {
        MotionEvent e = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, x, y, 0);
        e.setSource(InputDevice.SOURCE_TOUCHSCREEN);
        return e;
    }

    /**
     * Events are queued, not waited on: waiting for the app to finish
     * handling them (a BACK runs the whole transition) cost ~100 ms per
     * action, and the settle that follows waits for the screen anyway.
     */
    private static void taps(int x, int y, int n) throws InterruptedException {
        for (int k = 0; k < Math.max(1, n); k++) {
            if (k > 0) Thread.sleep(60);
            long down = SystemClock.uptimeMillis();
            if (!inject(touch(down, MotionEvent.ACTION_DOWN, x, y), false)) refused("tap");
            Thread.sleep(5);
            inject(touch(down, MotionEvent.ACTION_UP, x, y), false);
        }
    }

    private static void swipe(int x1, int y1, int x2, int y2, int ms) throws InterruptedException {
        long down = SystemClock.uptimeMillis();
        if (!inject(touch(down, MotionEvent.ACTION_DOWN, x1, y1), false)) refused("swipe");
        int steps = Math.max(1, ms / 10);
        for (int s = 1; s <= steps; s++) {
            Thread.sleep(Math.max(1, ms / steps));
            float f = (float) s / steps;
            inject(touch(down, MotionEvent.ACTION_MOVE, x1 + (x2 - x1) * f, y1 + (y2 - y1) * f), false);
        }
        inject(touch(down, MotionEvent.ACTION_UP, x2, y2), false);
    }

    private static void key(int code) {
        long now = SystemClock.uptimeMillis();
        if (!inject(new KeyEvent(now, now, KeyEvent.ACTION_DOWN, code, 0), false)) refused("key " + code);
        inject(new KeyEvent(now, SystemClock.uptimeMillis(), KeyEvent.ACTION_UP, code, 0), false);
    }

    private static void refused(String what) {
        throw new IllegalStateException(what + " refused: input injection failed");
    }

    /**
     * The focused field's text in one ACTION_SET_TEXT. "1": applied. "0":
     * refused, the field is as it was. "2 <hex>": applied, but the field
     * reads back differently (a mask reformatted it, or it is shorter than
     * the text): the text is in the field, so the host must not type it
     * again (VEL-164).
     */
    private static String setText(boolean append, String add) {
        AccessibilityNodeInfo focused = automation.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (focused == null) return "0";
        String wanted = add;
        if (append) {
            // An empty field reports its hint as its text (API 26+ says so).
            CharSequence current = focused.isShowingHintText() ? null : focused.getText();
            // A password field's text is its transformed form (bullets,
            // TextView.getTextForAccessibility): appending to it would put
            // the bullets in front. Refuse; the host types key events.
            if (focused.isPassword() && current != null && current.length() > 0) return "0";
            wanted = (current == null ? "" : current.toString()) + add;
        }
        // What a person typing could get in, as Appium does.
        String s = cut(wanted, focused.getMaxTextLength());
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, s);
        if (!focused.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) return "0";
        // A password field reads back as dots: nothing to compare.
        if (focused.isPassword()) return s.equals(wanted) ? "1" : "2 " + hex(s);
        // Read it back: a field may reformat (input masks).
        focused.refresh();
        CharSequence now = focused.isShowingHintText() ? "" : focused.getText();
        String got = now == null ? "" : now.toString();
        return got.equals(wanted) ? "1" : "2 " + hex(got);
    }

    /** At most `max` UTF-16 units (none when negative), never half a surrogate pair. */
    static String cut(String s, int max) {
        if (max < 0 || s.length() <= max) return s;
        if (max > 0 && Character.isHighSurrogate(s.charAt(max - 1))) max--;
        return s.substring(0, max);
    }

    private static boolean imeEnter() {
        if (android.os.Build.VERSION.SDK_INT < 30) return false;
        AccessibilityNodeInfo focused = automation.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (focused == null) return false;
        return focused.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.getId());
    }

    /**
     * The innermost scrollable node whose bounds hold (x, y) and that can
     * scroll along `axis` ("v", "h", or null for any): a node that says
     * which ways it scrolls (API 23+) and not this way is skipped, so a
     * vertical scroll over a carousel scrolls the page (VEL-S48).
     */
    private static AccessibilityNodeInfo scrollableAt(AccessibilityNodeInfo n, int x, int y, String axis) {
        if (n == null) return null;
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        if (!r.contains(x, y)) return null;
        for (int c = 0; c < n.getChildCount(); c++) {
            AccessibilityNodeInfo hit = scrollableAt(n.getChild(c), x, y, axis);
            if (hit != null) return hit;
        }
        if (!n.isScrollable()) return null;
        if (axis == null || scrollsAlong(n, axis) || (!scrollsAlong(n, "v") && !scrollsAlong(n, "h"))) {
            return n;
        }
        return null;
    }

    /** Whether `n` offers a directional scroll on `axis` (API 23+). */
    private static boolean scrollsAlong(AccessibilityNodeInfo n, String axis) {
        if (android.os.Build.VERSION.SDK_INT < 23) return false;
        int a = axis.equals("v")
                ? AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_UP.getId()
                : AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_LEFT.getId();
        int b = axis.equals("v")
                ? AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_DOWN.getId()
                : AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_RIGHT.getId();
        for (AccessibilityNodeInfo.AccessibilityAction act : n.getActionList()) {
            if (act.getId() == a || act.getId() == b) return true;
        }
        return false;
    }

    private static boolean scrollNode(int x, int y, boolean forward, String axis) throws InterruptedException {
        AccessibilityNodeInfo node = scrollableAt(activeRoot(), x, y, axis);
        if (node == null) return false;
        if (axis != null && scrollsAlong(node, axis)) {
            AccessibilityNodeInfo.AccessibilityAction act = axis.equals("v")
                    ? (forward ? AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_DOWN
                               : AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_UP)
                    : (forward ? AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_RIGHT
                               : AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_LEFT);
            return node.performAction(act.getId());
        }
        return node.performAction(forward
                ? AccessibilityNodeInfo.ACTION_SCROLL_FORWARD
                : AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD);
    }

    private static void type(String s) {
        if (s.isEmpty()) return;
        KeyEvent[] events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(s.toCharArray());
        if (events != null) {
            for (int k = 0; k < events.length; k++) {
                if (!inject(events[k], k == events.length - 1)) refused("typing");
            }
            return;
        }
        AccessibilityNodeInfo focused = automation.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (focused == null) throw new IllegalStateException("no focused field to type into");
        // An empty field reports its hint as its text (API 26+ says so).
        CharSequence current = focused.isShowingHintText() ? null : focused.getText();
        // A password field's text reads back as bullets (see setText).
        if (focused.isPassword() && current != null && current.length() > 0) refused("set text on a password field");
        Bundle args = new Bundle();
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                (current == null ? "" : current.toString()) + s);
        if (!focused.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)) refused("set text");
    }

    // ---- screenshots (A-9) ----------------------------------------------

    private static byte[] screenshot(String format, int quality) {
        Bitmap b = automation.takeScreenshot();
        if (b == null) throw new IllegalStateException("screenshot failed");
        ByteArrayOutputStream bytes = new ByteArrayOutputStream(512 * 1024);
        if (format.equals("jpeg")) {
            b.compress(Bitmap.CompressFormat.JPEG, quality, bytes);
        } else {
            b.compress(Bitmap.CompressFormat.PNG, 100, bytes);
        }
        b.recycle();
        return bytes.toByteArray();
    }

    // ---- windows (A-7) --------------------------------------------------

    /** H.264 encoders on this device, for the host's video choice. */
    private static String encoders() {
        StringBuilder sb = new StringBuilder();
        android.media.MediaCodecList list =
                new android.media.MediaCodecList(android.media.MediaCodecList.REGULAR_CODECS);
        for (android.media.MediaCodecInfo info : list.getCodecInfos()) {
            if (!info.isEncoder()) continue;
            for (String type : info.getSupportedTypes()) {
                if (!type.equalsIgnoreCase("video/avc")) continue;
                String hw = android.os.Build.VERSION.SDK_INT >= 29
                        ? (info.isHardwareAccelerated() ? "1" : "0") : "?";
                sb.append(info.getName()).append(' ').append(hw).append('\n');
            }
        }
        return sb.toString();
    }

    private static String windows() {
        List<AccessibilityWindowInfo> list = automation.getWindows();
        StringBuilder sb = new StringBuilder();
        // getWindows() is ordered top (front) first.
        for (AccessibilityWindowInfo w : list) {
            Rect r = new Rect();
            w.getBoundsInScreen(r);
            String pkg = "-";
            AccessibilityNodeInfo root = w.getRoot();
            if (root != null && root.getPackageName() != null) {
                pkg = root.getPackageName().toString();
            }
            sb.append(w.getLayer()).append(' ').append(w.getType()).append(' ')
                    .append(r.left).append(',').append(r.top).append(',')
                    .append(r.right).append(',').append(r.bottom).append(' ')
                    .append(pkg).append('\n');
        }
        return sb.toString();
    }

    // ---- tree -------------------------------------------------------------

    static synchronized String dump(boolean lite, boolean tags) throws InterruptedException {
        // A long-lived connection keeps an accessibility cache that can go
        // stale for good on Compose screens. Clearing it costs a full re-fetch
        // (~85 ms vs ~3 ms per dump, measured on API 34), so do it at most
        // once per CACHE_CLEAR_MS: staleness stays bounded below Velocity's
        // element-wait polling, and most dumps stay fast.
        long now = SystemClock.uptimeMillis();
        if (clearCache != null && now - lastClear >= CACHE_CLEAR_MS) {
            try { clearCache.invoke(automation); } catch (Exception ignored) { }
            lastClear = now;
        }
        AccessibilityNodeInfo root = activeRoot();
        if (root == null) throw new IllegalStateException("no active window");
        StringBuilder sb = new StringBuilder(64 * 1024);
        sb.append("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"")
                .append(rotation()).append('"');
        int attrsEnd = sb.length();
        sb.append('>');
        truncated = false;
        TAGS.start(System.nanoTime(), !lite && tags);
        node(sb, root, 0, lite, 0);
        for (AccessibilityNodeInfo pane : sidePanes(root)) node(sb, pane, 0, lite, 0);
        // Deeper nodes were left out: say so rather than pass for the whole tree.
        if (truncated) sb.insert(attrsEnd, " truncated=\"true\"");
        String toast = toastText;
        if (toast != null && SystemClock.uptimeMillis() - toastAt < TOAST_MS) {
            Rect screen = new Rect();
            root.getBoundsInScreen(screen);
            int h = Math.max(screen.height(), 1), w = Math.max(screen.width(), 1);
            sb.append("<node index=\"0\"");
            attr(sb, "text", toast);
            sb.append(" resource-id=\"\" class=\"android.widget.Toast\" content-desc=\"\" enabled=\"true\" visible-to-user=\"true\"")
              .append(" bounds=\"[").append(screen.left + w / 8).append(',').append(screen.top + h * 82 / 100)
              .append("][").append(screen.left + w * 7 / 8).append(',').append(screen.top + h * 90 / 100).append("]\" />");
        }
        sb.append("</hierarchy>");
        return sb.toString();
    }

    /** Nodes deeper than MAX_DEPTH were left out of the dump being built. */
    private static boolean truncated;

    /** The Compose delegate's extra-data key for a node's testTag (API 26+). */
    static final String TEST_TAG_KEY = "androidx.compose.ui.semantics.testTag";
    /** Tagged nodes read per dump: each read is one binder round trip. */
    static final int MAX_TAGS = TagReads.MAX_READS;
    /** Per-dump budget and per-node cache of the testTag reads. */
    private static final TagReads TAGS = new TagReads();

    /**
     * A Compose node's testTag, for an app that didn't set
     * testTagsAsResourceId (D-53): the delegate puts it in the extras only
     * when asked through refreshWithExtraData, and only with a non-null
     * Bundle. Only a node that offers the key (Compose's delegate does; a
     * View never does; checking is local, no IPC) and has no resource-id
     * is read. Each read is a binder call the app's main thread serves, so
     * reads are bounded to 50 ms per dump and cached per node across dumps
     * ({@link TagReads}). Null when there is none, the budget is spent, or
     * anything about the call fails (the dump never does because of it).
     */
    private static CharSequence testTag(AccessibilityNodeInfo n) {
        if (!TAGS.on() || android.os.Build.VERSION.SDK_INT < 26) return null;
        try {
            List<String> keys = n.getAvailableExtraData();
            if (keys == null || !keys.contains(TEST_TAG_KEY)) return null;
            long key = TagReads.key(n.getWindowId(), n.hashCode());
            long now = System.nanoTime();
            String known = TAGS.cached(key, now);
            if (known != null) return known.isEmpty() ? null : known;
            if (!TAGS.mayRead(now)) return null;
            CharSequence tag = null;
            if (n.refreshWithExtraData(TEST_TAG_KEY, new Bundle())) {
                Bundle extras = n.getExtras();
                tag = extras == null ? null : extras.getCharSequence(TEST_TAG_KEY);
            }
            TAGS.read(key, tag == null ? null : tag.toString(), System.nanoTime());
            return tag;
        } catch (RuntimeException e) {
            return null;
        }
    }

    /**
     * The default display's rotation (0-3), as UiDevice reads it; 0 when the
     * hidden API isn't there.
     */
    private static int rotation() {
        try {
            Class<?> dmg = Class.forName("android.hardware.display.DisplayManagerGlobal");
            Object global = dmg.getMethod("getInstance").invoke(null);
            Object display = dmg.getMethod("getRealDisplay", int.class).invoke(global, 0);
            return display == null ? 0 : ((android.view.Display) display).getRotation();
        } catch (Throwable ignored) {
            return 0;
        }
    }

    /**
     * The root to dump. The active window's root is null for a while after an
     * app is force-stopped and relaunched (the accessibility service still
     * points at the dead window); waiting 0.5 s and giving up made every read
     * fail for seconds, and those failures opened the circuit breaker
     * (VelocityLab, 2 runs in 3). Wait up to 3 s, as Appium waits for the
     * root, clearing the accessibility cache once, and meanwhile take the
     * front-most application window from the window list, as AndroidX's
     * getWindowRoots does.
     */
    private static AccessibilityNodeInfo activeRoot() throws InterruptedException {
        long deadline = SystemClock.uptimeMillis() + 3000;
        boolean cleared = false;
        while (true) {
            AccessibilityNodeInfo root = automation.getRootInActiveWindow();
            if (root != null) return root;
            try {
                for (AccessibilityWindowInfo w : automation.getWindows()) {
                    if (w.getType() != AccessibilityWindowInfo.TYPE_APPLICATION) continue;
                    AccessibilityNodeInfo r = w.getRoot();
                    if (r != null) return r;
                }
            } catch (RuntimeException ignored) {
            }
            if (!cleared && clearCache != null) {
                try { clearCache.invoke(automation); } catch (Exception ignored) { }
                cleared = true;
            }
            if (SystemClock.uptimeMillis() >= deadline) return null;
            Thread.sleep(50);
        }
    }

    /**
     * Other application windows shown beside the active one, not over or under
     * it: the panes of activity embedding (Settings on every tablet since
     * Android 12L puts its list and the open page in two windows) and split
     * screen. Only the active window used to be dumped, so the other pane was
     * invisible to Velocity (a POS tablet: "Internet" on screen, not found).
     * A window that overlaps the active one is left out: what a dialog covers
     * must stay hidden.
     */
    private static List<AccessibilityNodeInfo> sidePanes(AccessibilityNodeInfo activeRoot) {
        List<AccessibilityNodeInfo> panes = new java.util.ArrayList<>();
        Rect active = new Rect();
        activeRoot.getBoundsInScreen(active);
        List<Rect> taken = new java.util.ArrayList<>();
        taken.add(active);
        try {
            for (AccessibilityWindowInfo w : automation.getWindows()) {
                if (w.getType() != AccessibilityWindowInfo.TYPE_APPLICATION || w.isActive()) continue;
                Rect r = new Rect();
                w.getBoundsInScreen(r);
                if (r.isEmpty()) continue;
                boolean overlaps = false;
                for (Rect t : taken) if (Rect.intersects(t, r)) { overlaps = true; break; }
                if (overlaps) continue;
                AccessibilityNodeInfo root = w.getRoot();
                if (root == null) continue;
                taken.add(r);
                panes.add(root);
            }
        } catch (RuntimeException e) {
            // getWindows() needs the interactive-windows flag; without it, the
            // active window alone, as before.
        }
        return panes;
    }

    /**
     * Mirrors AccessibilityNodeInfoDumper: same attributes, invisible children
     * skipped. A lite dump (A-10) keeps only what a settle compares: text,
     * id, class, description, enabled/checked/selected/focused and bounds.
     */
    /** AccessibilityNodeInfoCompat's role description key: set by Compose and
     * by ViewCompat.setAccessibilityDelegate; read from the extras, which
     * every node already carries (no extra IPC). */
    static final String ROLE_DESCRIPTION_KEY = "AccessibilityNodeInfo.roleDescription";

    private static CharSequence roleDescription(AccessibilityNodeInfo n) {
        try {
            Bundle extras = n.getExtras();
            return extras == null ? null : extras.getCharSequence(ROLE_DESCRIPTION_KEY);
        } catch (RuntimeException e) {
            return null;
        }
    }

    private static void node(StringBuilder sb, AccessibilityNodeInfo n, int index, boolean lite, int depth) {
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        sb.append("<node index=\"").append(index).append('"');
        attr(sb, "text", n.getText());
        CharSequence resourceId = n.getViewIdResourceName();
        attr(sb, "resource-id", resourceId);
        if (!lite && (resourceId == null || resourceId.length() == 0)) {
            CharSequence tag = testTag(n);
            if (tag != null && tag.length() > 0) attr(sb, "test-tag", tag);
        }
        attr(sb, "class", n.getClassName());
        if (!lite) attr(sb, "package", n.getPackageName());
        attr(sb, "content-desc", n.getContentDescription());
        if (!lite) {
            // Protocol 9: an empty Flutter/Compose field is found by its
            // hint; state and error text say what a control shows.
            if (android.os.Build.VERSION.SDK_INT >= 26) {
                attr(sb, "hint", n.isShowingHintText() ? n.getText() : n.getHintText());
            }
            if (android.os.Build.VERSION.SDK_INT >= 30) attr(sb, "state-desc", n.getStateDescription());
            attr(sb, "error", n.getError());
            attr(sb, "role-desc", roleDescription(n));
            flag(sb, "checkable", n.isCheckable());
        }
        flag(sb, "checked", n.isChecked());
        if (!lite) flag(sb, "clickable", n.isClickable());
        flag(sb, "enabled", n.isEnabled());
        if (!lite) flag(sb, "focusable", n.isFocusable());
        flag(sb, "focused", n.isFocused());
        if (!lite) {
            flag(sb, "scrollable", n.isScrollable());
            flag(sb, "long-clickable", n.isLongClickable());
            flag(sb, "password", n.isPassword());
        }
        flag(sb, "selected", n.isSelected());
        sb.append(" bounds=\"[").append(r.left).append(',').append(r.top).append("][")
                .append(r.right).append(',').append(r.bottom).append("]\"");
        int start = sb.length();
        sb.append('>');
        if (depth >= MAX_DEPTH) {
            if (n.getChildCount() > 0) truncated = true;
        } else {
            children(sb, n, lite, depth + 1);
        }
        if (sb.length() == start + 1) {
            // No visible children: self-close, as uiautomator does.
            sb.setLength(start);
            sb.append(" />");
        } else {
            sb.append("</node>");
        }
    }

    /**
     * Visible children of {@code n}. An invisible child is skipped but its own
     * visible descendants are kept (lifted to this level): Compose interop
     * parents report invisible while their children are on screen, and
     * pruning the whole subtree hid them.
     */
    private static void children(StringBuilder sb, AccessibilityNodeInfo n, boolean lite, int depth) {
        int count = n.getChildCount();
        for (int i = 0; i < count; i++) {
            AccessibilityNodeInfo child = n.getChild(i);
            if (child == null) continue;
            if (child.isVisibleToUser()) {
                node(sb, child, i, lite, depth);
            } else if (depth >= MAX_DEPTH) {
                truncated = true;
            } else {
                children(sb, child, lite, depth + 1);
            }
            child.recycle();
        }
    }

    // ---- helpers --------------------------------------------------------

    private static void text(OutputStream out, String s) throws java.io.IOException {
        out.write((s + "\n").getBytes(StandardCharsets.UTF_8));
    }

    private static void payload(OutputStream out, byte[] bytes) throws java.io.IOException {
        out.write(("OK " + bytes.length + "\n").getBytes(StandardCharsets.UTF_8));
        out.write(bytes);
    }

    private static int i(String s) {
        return Integer.parseInt(s);
    }

    private static byte[] unhex(String s) {
        byte[] b = new byte[s.length() / 2];
        for (int k = 0; k < b.length; k++) {
            b[k] = (byte) Integer.parseInt(s.substring(2 * k, 2 * k + 2), 16);
        }
        return b;
    }

    private static String hex(String s) {
        StringBuilder h = new StringBuilder();
        for (byte b : s.getBytes(StandardCharsets.UTF_8)) h.append(String.format("%02x", b & 0xff));
        return h.toString();
    }

    private static String oneLine(Throwable e) {
        Throwable t = e instanceof java.lang.reflect.InvocationTargetException && e.getCause() != null
                ? e.getCause() : e;
        String m = t.getClass().getSimpleName() + ": " + t.getMessage();
        return m.replace('\n', ' ');
    }

    private static void flag(StringBuilder sb, String name, boolean v) {
        sb.append(' ').append(name).append("=\"").append(v).append('"');
    }

    private static void attr(StringBuilder sb, String name, CharSequence value) {
        sb.append(' ').append(name).append("=\"");
        if (value != null) {
            for (int i = 0; i < value.length(); i++) {
                char c = value.charAt(i);
                switch (c) {
                    case '&': sb.append("&amp;"); break;
                    case '<': sb.append("&lt;"); break;
                    case '>': sb.append("&gt;"); break;
                    case '"': sb.append("&quot;"); break;
                    case '\'': sb.append("&apos;"); break;
                    case '\n': sb.append("&#10;"); break;
                    case '\r': sb.append("&#13;"); break;
                    case '\t': sb.append("&#9;"); break;
                    default:
                        // XML 1.0 forbids most control characters, U+FFFE,
                        // U+FFFF and unpaired surrogates (a parser rejects
                        // the whole dump for one).
                        if (Character.isHighSurrogate(c)) {
                            if (i + 1 < value.length() && Character.isLowSurrogate(value.charAt(i + 1))) {
                                sb.append(c).append(value.charAt(++i));
                            }
                        } else if (Character.isLowSurrogate(c) || c == 0xFFFE || c == 0xFFFF) {
                            // dropped
                        } else if (c >= 0x20) {
                            sb.append(c);
                        }
                }
            }
        }
        sb.append('"');
    }
}
