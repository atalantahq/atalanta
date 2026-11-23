// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react;

/** Host-check fake: getReactInstanceManager() creates one, as in RN (the probe must not call it first). */
public class ReactNativeHost {
    public ReactInstanceManager manager;
    public int created;

    public boolean hasInstance() {
        return manager != null;
    }

    public ReactInstanceManager getReactInstanceManager() {
        if (manager == null) {
            created++;
            manager = new ReactInstanceManager();
        }
        return manager;
    }
}
