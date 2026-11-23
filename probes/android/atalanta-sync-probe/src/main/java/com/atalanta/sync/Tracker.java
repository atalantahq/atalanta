// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.util.Collections;
import java.util.List;

/**
 * A source of pending work the probe asks on every sample (timers on the
 * main looper, React Native's queues). Unlike an {@code IdlingResource} it
 * answers with a count and can take the asking connection's timer horizon.
 */
public interface Tracker {
    /** {@link #pending} when the tracker can't tell on this device. */
    int UNKNOWN = -1;

    /** Shown in {@code pending} as {@code name:count}, and in {@code unknown}. */
    String name();

    /**
     * Pending work right now: 0 is idle, {@link #UNKNOWN} is "can't tell"
     * (never idle by accident: an unknown tracker is reported, not trusted).
     * {@code maxTimerMs} is the asking connection's {@code max_timer_ms}, -1
     * when it set none.
     */
    int pending(long maxTimerMs);

    /**
     * Kinds of perpetual work this tracker saw on its last sample and no
     * longer counts (a poller, a looping animation, a stream open for
     * long): reported as {@code ignored: perpetual <kind>}, never waited for.
     */
    default List<String> ignoredPerpetual() {
        return Collections.emptyList();
    }
}
