package com.facebook.react.animated;

import android.util.SparseArray;

/**
 * Host-check fake mirroring RN 0.81 {@code NativeAnimatedNodesManager.kt}:
 * {@code private val activeAnimations = SparseArray<AnimationDriver>()},
 * {@code updatedNodes}, and {@code hasActiveAnimations()} over both.
 */
public class NativeAnimatedNodesManager {
    private final SparseArray<AnimationDriver> activeAnimations = new SparseArray<>();
    private final SparseArray<Object> updatedNodes = new SparseArray<>();

    public boolean hasActiveAnimations() {
        return activeAnimations.size() > 0 || updatedNodes.size() > 0;
    }

    /** startAnimatingNode: a frame-based driver with these iterations (-1: forever). */
    public FrameBasedAnimationDriver start(int id, int iterations) {
        FrameBasedAnimationDriver d = new FrameBasedAnimationDriver(iterations);
        d.id = id;
        activeAnimations.put(id, d);
        return d;
    }

    public void finish(int id) {
        activeAnimations.remove(id);
    }

    public void touchNode(int tag, boolean on) {
        if (on) updatedNodes.put(tag, new Object()); else updatedNodes.remove(tag);
    }
}
