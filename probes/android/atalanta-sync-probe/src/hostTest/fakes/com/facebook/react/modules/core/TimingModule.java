package com.facebook.react.modules.core;

import com.facebook.react.module.annotations.ReactModule;

/**
 * Host-check fake mirroring RN 0.77/0.81 {@code TimingModule.kt}:
 * {@code private val javaTimerManager} and the public
 * {@code @VisibleForTesting fun hasActiveTimersInRange}. Old architecture
 * only: bridgeless's CoreReactPackage doesn't register it.
 */
@ReactModule(name = "Timing")
public class TimingModule {
    private final JavaTimerManager javaTimerManager;

    public TimingModule(JavaTimerManager manager) {
        this.javaTimerManager = manager;
    }

    public boolean hasActiveTimersInRange(long rangeMs) {
        return javaTimerManager.hasActiveTimersInRange$ReactAndroid_release(rangeMs);
    }
}
