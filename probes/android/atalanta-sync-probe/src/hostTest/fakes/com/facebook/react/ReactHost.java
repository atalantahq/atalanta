// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.facebook.react;

import com.facebook.react.bridge.ReactContext;

/** Host-check fake of RN's ReactHost interface. */
public interface ReactHost {
    ReactContext getCurrentReactContext();
}
