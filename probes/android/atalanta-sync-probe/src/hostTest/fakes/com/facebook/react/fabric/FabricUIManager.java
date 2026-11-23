package com.facebook.react.fabric;

import com.facebook.react.fabric.mounting.MountItemDispatcher;

/** Host-check fake. */
public class FabricUIManager {
    private final MountItemDispatcher mMountItemDispatcher = new MountItemDispatcher();

    public MountItemDispatcher dispatcher() {
        return mMountItemDispatcher;
    }
}
