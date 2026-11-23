package com.atalanta.sync;

import java.util.List;

/** What the sync server reports. Implemented by the Kotlin {@code AppStateTracker}. */
public interface ProbeState {
    /** Idle right now (one sample). */
    boolean idleNow();

    /** Why not idle, e.g. "network:1", "main_looper". */
    List<String> pendingNow();

    /**
     * {@link #idleNow()} for a host connection that set {@code max_timer_ms}
     * (-1: not set): timers due further out than that don't count. The
     * Android probe tracks no timers yet, so the default ignores it.
     */
    default boolean idleNow(long maxTimerMs) {
        return idleNow();
    }

    /** {@link #pendingNow()} with a connection's {@code max_timer_ms}. */
    default List<String> pendingNow(long maxTimerMs) {
        return pendingNow();
    }

    /** Trackers that can't tell on this device, e.g. "animation". */
    List<String> unknownTrackers();

    /**
     * A host connection's {@code config} named the app's framework (e.g.
     * {@code "react_native"}): turn on its trackers. False when this probe
     * has no tracking for it.
     */
    default boolean enableFramework(String framework) {
        return false;
    }

    /**
     * {@code ignored: perpetual <kind>}: perpetual work no longer counted
     * (reported in {@code pending} even when idle).
     */
    default List<String> ignoredNow() {
        return Trackers.SHARED.ignored();
    }

    /**
     * {@code config} {@code ignore_trackers}: trackers that never count
     * (process-wide; replaces the last list). Returns the names this probe
     * doesn't have.
     */
    default List<String> optOutTrackers(List<String> names) {
        return Trackers.SHARED.optOut(names);
    }
}
