package com.facebook.react.module.annotations;

import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;

/** Host-check fake of RN's @ReactModule (runtime retention, as RN's). */
@Retention(RetentionPolicy.RUNTIME)
public @interface ReactModule {
    String name();
}
