package com.facebook.react.runtime;

import com.facebook.react.ReactHost;
import com.facebook.react.bridge.ReactContext;

/** Host-check fake of the bridgeless host. */
public class ReactHostImpl implements ReactHost {
    public volatile ReactContext context;
    private ReactInstance mReactInstance;

    public void setInstance(ReactInstance i) {
        mReactInstance = i;
    }

    @Override
    public ReactContext getCurrentReactContext() {
        return context;
    }

    @Override
    public String toString() {
        return "ReactHostImpl(" + mReactInstance + ")";
    }
}
