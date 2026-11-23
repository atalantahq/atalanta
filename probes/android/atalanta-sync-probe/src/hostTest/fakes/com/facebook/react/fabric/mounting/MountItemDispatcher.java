// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react.fabric.mounting;

import java.util.concurrent.ConcurrentLinkedQueue;

/** Host-check fake with RN's (Java) queue names. */
public class MountItemDispatcher {
    private final ConcurrentLinkedQueue<Object> mViewCommandMountItems = new ConcurrentLinkedQueue<>();
    private final ConcurrentLinkedQueue<Object> mMountItems = new ConcurrentLinkedQueue<>();
    private final ConcurrentLinkedQueue<Object> mPreMountItems = new ConcurrentLinkedQueue<>();

    public void addMountItem(Object o) { mMountItems.add(o); }
    public void addViewCommand(Object o) { mViewCommandMountItems.add(o); }
    public void addPreMount(Object o) { mPreMountItems.add(o); }

    public void dispatchAll() {
        mMountItems.clear();
        mViewCommandMountItems.clear();
    }
}
