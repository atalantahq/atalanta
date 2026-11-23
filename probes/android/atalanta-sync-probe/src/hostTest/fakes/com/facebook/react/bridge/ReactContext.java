// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react.bridge;

import java.util.HashMap;
import java.util.Map;
import java.util.function.Supplier;

/**
 * Host-check fake of ReactContext: native modules by class, made lazily by
 * {@code getNativeModule} (as RN's TurboModuleManager / ModuleHolder do);
 * {@code hasNativeModule} is true only once made (bridgeless
 * {@code TurboModuleManager.hasModule}); the Fabric UIManager.
 */
public class ReactContext {
    public final Map<Class<?>, Object> modules = new HashMap<>();
    public final Map<Class<?>, Supplier<?>> lazy = new HashMap<>();
    public Object fabric;
    public int createdLazily;

    public <T> boolean hasNativeModule(Class<T> cls) {
        return modules.containsKey(cls);
    }

    public <T> T getNativeModule(Class<T> cls) {
        Object m = modules.get(cls);
        if (m == null) {
            Supplier<?> make = lazy.remove(cls);
            if (make == null) throw new IllegalStateException("module not registered: " + cls.getName());
            createdLazily++;
            m = make.get();
            modules.put(cls, m);
        }
        return cls.cast(m);
    }

    public Object getFabricUIManager() {
        return fabric;
    }
}
