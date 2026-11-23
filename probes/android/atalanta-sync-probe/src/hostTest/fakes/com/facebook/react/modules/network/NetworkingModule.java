package com.facebook.react.modules.network;

import com.facebook.react.module.annotations.ReactModule;
import okhttp3.OkHttpClient;

/** Host-check fake: RN's NetworkingModule holds its own OkHttpClient ({@code mClient} in 0.77, {@code client} in 0.81). */
@ReactModule(name = "Networking")
public class NetworkingModule {
    private final OkHttpClient mClient = new OkHttpClient();

    public OkHttpClient client() {
        return mClient;
    }
}
