// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okhttp3;

/** Host-check stand-in for OkHttp's Response: {@code body()}, {@code newBuilder().body(..).build()}, {@code close()}. */
public final class Response implements java.io.Closeable {
    public final int code;
    private final ResponseBody body;

    public Response(int code) {
        this(code, null);
    }

    public Response(int code, ResponseBody body) {
        this.code = code;
        this.body = body;
    }

    public ResponseBody body() {
        return body;
    }

    public Builder newBuilder() {
        return new Builder(this);
    }

    /** As OkHttp: closes the body. */
    @Override
    public void close() {
        if (body != null) body.close();
    }

    public static final class Builder {
        private final int code;
        private ResponseBody body;

        Builder(Response r) {
            this.code = r.code;
            this.body = r.body;
        }

        public Builder body(ResponseBody body) {
            this.body = body;
            return this;
        }

        public Response build() {
            return new Response(code, body);
        }
    }
}
