// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react;

import com.facebook.react.bridge.ReactContext;

/** Host-check fake. */
public class ReactInstanceManager {
    public volatile ReactContext context;

    public ReactContext getCurrentReactContext() {
        return context;
    }
}
