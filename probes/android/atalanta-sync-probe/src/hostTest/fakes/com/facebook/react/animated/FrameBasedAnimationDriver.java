// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react.animated;

/** Host-check fake of RN's {@code FrameBasedAnimationDriver}: {@code private var iterations}, {@code currentLoop}. */
public class FrameBasedAnimationDriver extends AnimationDriver {
    private int iterations;
    private int currentLoop = 1;

    FrameBasedAnimationDriver(int iterations) {
        this.iterations = iterations;
    }

    /** A loop finished and the driver went round again (iterations == -1 or not yet reached). */
    public void loopAgain() {
        currentLoop++;
    }
}
