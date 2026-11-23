package com.atalanta.sync;

import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/** Logcat (or stderr on the host JVM), with one-time warnings. */
final class ProbeLog {
    static final String TAG = "AtalantaSyncProbe";
    private static final Set<String> WARNED = ConcurrentHashMap.newKeySet();

    private ProbeLog() {}

    static void info(String msg) {
        try {
            android.util.Log.i(TAG, msg);
        } catch (Throwable t) {
            // host JVM (android.jar stubs throw)
            System.err.println(TAG + ": " + msg);
        }
    }

    /** Logged the first time {@code key} is seen; returns whether it was. */
    static boolean warnOnce(String key, String msg) {
        if (!WARNED.add(key)) return false;
        try {
            android.util.Log.w(TAG, msg);
        } catch (Throwable t) {
            System.err.println(TAG + " [warn]: " + msg);
        }
        return true;
    }

    /** For the host checks: whether {@code key} was warned about. */
    static boolean warned(String key) {
        return WARNED.contains(key);
    }
}
