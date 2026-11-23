package com.atalanta.sync;

import java.io.IOException;
import okhttp3.Interceptor;
import okhttp3.MediaType;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.ResponseBody;
import okio.Buffer;
import okio.BufferedSource;
import okio.ForwardingSource;
import okio.Okio;

/**
 * Reports an OkHttp client's requests to the probe, so the app counts as
 * busy while one is in flight. Add it to the app's client(s) in debug
 * builds:
 *
 * <pre>
 *   OkHttpClient client = new OkHttpClient.Builder()
 *       .addInterceptor(new AtalantaOkHttpInterceptor())
 *       .build();
 * </pre>
 *
 * A request counts from the moment the interceptor sees it until its
 * response body is done: read to the end, closed (also through
 * {@code Response.close()}), or failed while reading; exactly once. With
 * Retrofit / {@code enqueue} the body is downloaded and parsed after
 * {@code proceed} returns, so counting only until the headers arrived read
 * as idle while the data was still coming. A request that fails, or whose
 * response has no body, ends right away. WebSocket and long-poll URLs
 * never count ({@link UrlBlocklist}); a body the app never finishes stops
 * counting {@link NetworkTracker#BODY_MAX_MS} after the headers arrived, with
 * a warning (OkHttp reports that leak too).
 *
 * <p>The probe declares OkHttp {@code compileOnly}: this class loads only
 * in an app that has OkHttp (3.x or 4.x, with Okio) and refers to it. React
 * Native apps need no interceptor for {@code fetch}: with {@code framework:
 * react_native} the probe reads RN's own client ({@link ReactNativeIdling}).
 */
public final class AtalantaOkHttpInterceptor implements Interceptor {
    private final NetworkTracker tracker;

    public AtalantaOkHttpInterceptor() {
        this(NetworkTracker.SHARED);
    }

    public AtalantaOkHttpInterceptor(NetworkTracker tracker) {
        this.tracker = tracker;
    }

    @Override
    public Response intercept(Interceptor.Chain chain) throws IOException {
        Request request = chain.request();
        NetworkTracker.Handle handle;
        try {
            handle = tracker.open(String.valueOf(request.url()));
        } catch (Throwable t) {
            // Never fail the app's request over the probe.
            handle = null;
        }
        if (handle == null) return chain.proceed(request);
        Response response;
        try {
            response = chain.proceed(request);
        } catch (IOException | RuntimeException | Error e) {
            handle.close();
            throw e;
        }
        try {
            ResponseBody body = response.body();
            if (body == null) {
                handle.close();
                return response;
            }
            Response counted = response.newBuilder().body(new CountedBody(body, handle)).build();
            handle.bodyStarted();
            return counted;
        } catch (Throwable t) {
            handle.close();
            return response;
        }
    }

    /** The response body, ending the request on EOF, close or a read error. */
    static final class CountedBody extends ResponseBody {
        private final ResponseBody body;
        private final NetworkTracker.Handle handle;
        private BufferedSource source;

        CountedBody(ResponseBody body, NetworkTracker.Handle handle) {
            this.body = body;
            this.handle = handle;
        }

        @Override
        public MediaType contentType() {
            return body.contentType();
        }

        @Override
        public long contentLength() {
            return body.contentLength();
        }

        @Override
        public synchronized BufferedSource source() {
            if (source == null) {
                source = Okio.buffer(new ForwardingSource(body.source()) {
                    @Override
                    public long read(Buffer sink, long byteCount) throws IOException {
                        long n;
                        try {
                            n = super.read(sink, byteCount);
                        } catch (IOException | RuntimeException | Error e) {
                            handle.close();
                            throw e;
                        }
                        if (n == -1) handle.close();
                        else if (n > 0) handle.bodyProgress();
                        return n;
                    }

                    @Override
                    public void close() throws IOException {
                        try {
                            super.close();
                        } finally {
                            handle.close();
                        }
                    }
                });
            }
            return source;
        }

        @Override
        public void close() {
            try {
                body.close();
            } finally {
                handle.close();
            }
        }
    }
}
