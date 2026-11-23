// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react.animated;

import com.facebook.react.module.annotations.ReactModule;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Host-check fake mirroring RN 0.81 {@code NativeAnimatedModule.kt}:
 * {@code private val nodesManagerRef = AtomicReference<NativeAnimatedNodesManager?>()}
 * (RN 0.77's Java spelling is {@code mNodesManager}), created lazily by
 * {@code getNodesManager()}.
 */
@ReactModule(name = "NativeAnimatedModule")
public class NativeAnimatedModule {
    private final AtomicReference<NativeAnimatedNodesManager> nodesManagerRef = new AtomicReference<>();
    public int created;

    public NativeAnimatedNodesManager getNodesManager() {
        if (nodesManagerRef.get() == null) {
            created++;
            nodesManagerRef.set(new NativeAnimatedNodesManager());
        }
        return nodesManagerRef.get();
    }
}
