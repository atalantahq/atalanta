package com.facebook.react.runtime;

import com.facebook.react.modules.core.JavaTimerManager;

/** Host-check fake with the Kotlin-style field name (the probe tries both spellings). */
public class ReactInstance {
    private final JavaTimerManager javaTimerManager;

    public ReactInstance(JavaTimerManager m) {
        this.javaTimerManager = m;
    }

    @Override
    public String toString() {
        return "ReactInstance(" + javaTimerManager + ")";
    }
}
