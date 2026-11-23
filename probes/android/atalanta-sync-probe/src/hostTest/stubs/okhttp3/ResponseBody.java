// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okhttp3;

import okio.BufferedSource;

/** Host-check stand-in for OkHttp's ResponseBody (abstract contentType/contentLength/source; close closes source()). */
public abstract class ResponseBody implements java.io.Closeable {
    public abstract MediaType contentType();

    public abstract long contentLength();

    public abstract BufferedSource source();

    @Override
    public void close() {
        try {
            source().close();
        } catch (Exception ignored) {
            // closeQuietly
        }
    }

    /** A body over these bytes (OkHttp's ResponseBody.create). */
    public static ResponseBody create(MediaType type, byte[] bytes) {
        final BufferedSource s = okio.Okio.buffer(new okio.Buffer().write(bytes));
        return new ResponseBody() {
            @Override public MediaType contentType() { return type; }
            @Override public long contentLength() { return bytes.length; }
            @Override public BufferedSource source() { return s; }
        };
    }
}
