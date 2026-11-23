// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okio;

import java.io.IOException;

/** Host-check stand-in for Okio's {@code Okio.buffer(Source)} (okio 1.x-3.x from Java). */
public final class Okio {
    private Okio() {}

    public static BufferedSource buffer(final Source source) {
        return new BufferedSource() {
            @Override public long read(Buffer sink, long byteCount) throws IOException { return source.read(sink, byteCount); }
            @Override public Timeout timeout() { return source.timeout(); }
            @Override public void close() throws IOException { source.close(); }
            @Override public String readUtf8() throws IOException { return Buffer.drain(source); }
        };
    }
}
