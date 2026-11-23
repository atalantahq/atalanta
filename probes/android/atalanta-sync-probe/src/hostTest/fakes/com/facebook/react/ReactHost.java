package com.facebook.react;

import com.facebook.react.bridge.ReactContext;

/** Host-check fake of RN's ReactHost interface. */
public interface ReactHost {
    ReactContext getCurrentReactContext();
}
