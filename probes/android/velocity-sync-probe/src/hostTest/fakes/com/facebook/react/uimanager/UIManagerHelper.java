package com.facebook.react.uimanager;

import com.facebook.react.bridge.ReactContext;

/** Host-check fake: UIManagerHelper.getUIManager(context, FABRIC). */
public final class UIManagerHelper {
    private UIManagerHelper() {}

    public static Object getUIManager(ReactContext context, int type) {
        return type == 2 ? context.getFabricUIManager() : null;
    }
}
