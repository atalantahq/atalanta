package com.atalanta.sync;

import java.lang.reflect.Method;

/**
 * Running animator count through the framework's hidden counter:
 * {@code ValueAnimator.getCurrentAnimationsCount()} (AOSP
 * {@code ValueAnimator.java:1722}), else
 * {@code AnimationHandler.getAnimationCount()}. The probe used to reflect
 * {@code getRunningAnimationCount}, which doesn't exist, and took the
 * exception for "idle", so animation sync never worked.
 *
 * <p>The counter is per-thread: it reads {@code AnimationHandler}'s
 * {@code ThreadLocal} (AOSP {@code AnimationHandler.java:113, 447-456}), so
 * from the probe's socket thread it is always 0 ("may be called on arbitrary
 * threads" means only that the call is safe). With a main-thread poster the
 * count is read on the main thread; when the main thread can't answer within
 * {@link MainThreadCheck#BUDGET_MS} the answer is {@link #MAIN_THREAD_BUSY}.
 *
 * When neither can be read the answer is {@link #UNKNOWN}, never 0, unless
 * animators are disabled (duration scale 0, as the Atalanta driver sets it):
 * then every animation ends at once and 0 is the truth.
 */
public final class AnimationCounter {
    public static final int UNKNOWN = -1;
    /** The main thread didn't run the read in time: busy, never 0. */
    public static final int MAIN_THREAD_BUSY = Integer.MAX_VALUE;
    private static final int FAILED = Integer.MIN_VALUE;

    /** Whether animators run at all ({@code ValueAnimator.areAnimatorsEnabled}, API 26). */
    public interface Enabled {
        /** null when not known. */
        Boolean animatorsEnabled();
    }

    private final Method method;
    private final Enabled enabled;
    private final MainThreadSampler onMain;

    /**
     * Candidates are tried in order: {class, static no-arg method name}.
     * Reads on the calling thread: only right when that is the thread whose
     * animators count (tests, or a caller already on the main thread).
     */
    public AnimationCounter(Object[][] candidates, Enabled enabled) {
        this(candidates, enabled, null);
    }

    /** Reads on the thread {@code mainPoster} posts to, when not null. */
    public AnimationCounter(Object[][] candidates, Enabled enabled, MainThreadCheck.Poster mainPoster) {
        this.method = find(candidates);
        this.enabled = enabled;
        this.onMain = mainPoster == null
                ? null
                : new MainThreadSampler(mainPoster, this::invoke, MainThreadCheck.BUDGET_MS, FAILED);
    }

    /** The real framework candidates, read on the main thread through {@code mainPoster}. */
    public static AnimationCounter forFramework(MainThreadCheck.Poster mainPoster) {
        Object[][] candidates = {
            {"android.animation.ValueAnimator", "getCurrentAnimationsCount"},
            {"android.animation.AnimationHandler", "getAnimationCount"},
        };
        return new AnimationCounter(candidates, AnimationCounter::frameworkEnabled, mainPoster);
    }

    private static Boolean frameworkEnabled() {
        try {
            Method m = Class.forName("android.animation.ValueAnimator").getMethod("areAnimatorsEnabled");
            Object v = m.invoke(null);
            return v instanceof Boolean ? (Boolean) v : null;
        } catch (Throwable t) {
            return null;
        }
    }

    private static Method find(Object[][] candidates) {
        for (Object[] c : candidates) {
            try {
                Class<?> cls = c[0] instanceof Class ? (Class<?>) c[0] : Class.forName((String) c[0]);
                Method m = cls.getDeclaredMethod((String) c[1]);
                m.setAccessible(true);
                return m;
            } catch (Throwable t) {
                // next candidate
            }
        }
        return null;
    }

    /** Whether a counter method was found. */
    public boolean available() {
        return method != null;
    }

    /** Running animations, {@link #UNKNOWN}, or {@link #MAIN_THREAD_BUSY}. */
    public int count() {
        if (method != null) {
            int v = onMain == null ? invoke() : onMain.read(MAIN_THREAD_BUSY);
            if (v != FAILED) return v;
        }
        Boolean on = enabled == null ? null : enabled.animatorsEnabled();
        return Boolean.FALSE.equals(on) ? 0 : UNKNOWN;
    }

    private int invoke() {
        try {
            Object v = method.invoke(null);
            if (v instanceof Number) return ((Number) v).intValue();
        } catch (Throwable t) {
            // the scale check decides
        }
        return FAILED;
    }
}
