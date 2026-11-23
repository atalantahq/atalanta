// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react.defaults;

import com.facebook.react.ReactHost;
import java.util.function.Supplier;

/**
 * Host-check fake of RN's {@code object DefaultReactHost}: a Kotlin object's
 * {@code private var reactHost} is a private static field; getDefaultReactHost
 * builds the host on first call (unsynchronised), as RN 0.77/0.81 do.
 */
public final class DefaultReactHost {
    private static ReactHost reactHost;
    public static int created;

    private DefaultReactHost() {}

    public static ReactHost getDefaultReactHost(Supplier<ReactHost> make) {
        if (reactHost == null) {
            created++;
            reactHost = make.get();
        }
        return reactHost;
    }

    /** For the host checks. */
    public static void reset() {
        reactHost = null;
        created = 0;
    }
}
