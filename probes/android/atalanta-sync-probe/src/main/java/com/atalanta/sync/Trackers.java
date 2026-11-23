// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CopyOnWriteArraySet;

/**
 * The probe's counting trackers ({@link Tracker}): main-looper timers
 * always, React Native's queues once a host connection says
 * {@code "framework":"react_native"} in {@code config}.
 *
 * <p>A tracker that can't tell answers {@link Tracker#UNKNOWN}: it never
 * holds the app busy and never reads as idle work done; it is listed in
 * {@code unknown} instead (with one warning in logcat).
 */
public final class Trackers {
    public static final Trackers SHARED = new Trackers();

    /** The frameworks {@code config} takes. */
    public static final String REACT_NATIVE = "react_native";

    private final List<Tracker> trackers = new CopyOnWriteArrayList<>();
    /** Each tracker's last answer was UNKNOWN. */
    private final Map<Tracker, Boolean> lastUnknown = new ConcurrentHashMap<>();
    private volatile Object application;
    private volatile ReactNativeIdling reactNative;
    /** Trackers opted out by name ({@code config} {@code ignore_trackers}): never counted. */
    private final Set<String> optedOut = new CopyOnWriteArraySet<>();

    /**
     * The app's {@code Application} (for React Native's {@code ReactApplication}).
     * React Native tracking asked for before this ({@code trackReactNative()}
     * before {@code start(context)}) is turned on now.
     */
    public synchronized void setApplication(Object app) {
        application = app;
        ReactNativeIdling rn = reactNative;
        if (app != null && rn != null && !rn.hasApplication()) {
            // Turned on before start(context): that instance can never see the app.
            for (Tracker t : rn.trackersCreated()) remove(t);
            reactNative = null;
            enableFramework(REACT_NATIVE);
        }
    }

    /**
     * Trackers (by name: {@code timer}, {@code rn_timers}, {@code rn_animation},
     * ...) that never count, process-wide; replaces the previous set.
     * Returns the names not known to this probe (still applied: a tracker
     * turned on later is opted out too).
     */
    public List<String> optOut(List<String> names) {
        optedOut.clear();
        List<String> unknownNames = new ArrayList<>();
        for (String n : names) {
            if (n == null || n.isEmpty()) continue;
            optedOut.add(n);
            if (!KNOWN.contains(n)) unknownNames.add(n);
        }
        return unknownNames;
    }

    /** The trackers opted out. */
    public List<String> optedOut() {
        return new ArrayList<>(optedOut);
    }

    /** Tracker names this probe has (for {@code ignore_trackers}). */
    static final List<String> KNOWN = Arrays.asList(
            "timer", "rn_loading", "rn_timers", "rn_mounting", "rn_animation", "rn_network");

    public void add(Tracker t) {
        if (t != null && !trackers.contains(t)) trackers.add(t);
    }

    public void remove(Tracker t) {
        trackers.remove(t);
        lastUnknown.remove(t);
    }

    /** Drop every tracker (the probe stopped). */
    public synchronized void clear() {
        trackers.clear();
        lastUnknown.clear();
        reactNative = null;
    }

    /**
     * Turn on a framework's trackers (process-wide: the app is what it is).
     * Returns false for a framework the probe doesn't know; React Native in
     * an app without {@code ReactApplication} turns on nothing and says so
     * in {@code unknown}.
     */
    public synchronized boolean enableFramework(String framework) {
        if (!REACT_NATIVE.equals(framework)) return false;
        if (reactNative != null) return true;
        ReactNativeIdling rn = new ReactNativeIdling(application, UrlBlocklist.SHARED, ReactNativeIdling.SYSTEM_CLOCK);
        reactNative = rn;
        for (Tracker t : rn.trackers()) add(t);
        return true;
    }

    /** Whether React Native tracking is on. */
    public boolean reactNativeEnabled() {
        return reactNative != null;
    }

    /** Nothing pending on any tracker that can tell. */
    public boolean idle(long maxTimerMs) {
        for (Tracker t : trackers) {
            if (optedOut.contains(t.name())) continue;
            if (sample(t, maxTimerMs) > 0) return false;
        }
        return true;
    }

    /** {@code name:count} for each tracker with pending work. */
    public List<String> pending(long maxTimerMs) {
        List<String> out = new ArrayList<>();
        for (Tracker t : trackers) {
            if (optedOut.contains(t.name())) continue;
            int n = sample(t, maxTimerMs);
            if (n > 0) out.add(t.name() + ":" + n);
        }
        out.addAll(ignored());
        return out;
    }

    /**
     * {@code ignored: perpetual <kind>} for each kind of perpetual work a
     * tracker stopped counting (as of its last sample). Reported in
     * {@code pending} whether or not the app is idle; never holds it busy.
     */
    public List<String> ignored() {
        List<String> out = new ArrayList<>();
        for (Tracker t : trackers) {
            if (optedOut.contains(t.name())) continue;
            List<String> kinds;
            try {
                kinds = t.ignoredPerpetual();
            } catch (Throwable e) {
                continue;
            }
            for (String k : kinds) {
                String note = "ignored: perpetual " + k;
                if (!out.contains(note)) out.add(note);
            }
        }
        return out;
    }

    /** The trackers whose last answer was "can't tell". */
    public List<String> unknown() {
        List<String> out = new ArrayList<>();
        for (Tracker t : trackers) {
            if (optedOut.contains(t.name())) continue;
            Boolean u = lastUnknown.get(t);
            if (u == null) u = sample(t, -1) == Tracker.UNKNOWN;
            if (u) out.add(t.name());
        }
        return out.isEmpty() ? Collections.<String>emptyList() : out;
    }

    private int sample(Tracker t, long maxTimerMs) {
        int n;
        try {
            n = t.pending(maxTimerMs);
        } catch (Throwable e) {
            // A tracker never takes the probe (or the app) down.
            ProbeLog.warnOnce("tracker-throw:" + t.name(),
                    t.name() + " tracking failed (" + e + "); not waited for");
            n = Tracker.UNKNOWN;
        }
        lastUnknown.put(t, n == Tracker.UNKNOWN);
        return n;
    }
}
