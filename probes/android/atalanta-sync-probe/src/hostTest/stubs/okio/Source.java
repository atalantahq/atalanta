// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okio;

import java.io.IOException;

/** Host-check stand-in for Okio's Source. */
public interface Source extends java.io.Closeable {
    long read(Buffer sink, long byteCount) throws IOException;

    Timeout timeout();

    @Override
    void close() throws IOException;
}
