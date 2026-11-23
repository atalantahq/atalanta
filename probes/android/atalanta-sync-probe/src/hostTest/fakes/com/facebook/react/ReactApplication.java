// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react;

/** Host-check fake of RN's ReactApplication (both architectures' getters). */
public interface ReactApplication {
    ReactNativeHost getReactNativeHost();

    /** Bridgeless; RN's default throws when the app doesn't implement it. */
    default ReactHost getReactHost() {
        throw new RuntimeException("You should either implement getReactNativeHost or getReactHost");
    }
}
