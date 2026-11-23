// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okhttp3;

/** Host-check stand-in for OkHttp's Call. */
public final class Call {
    private final Request request;

    public Call(String url) {
        this.request = new Request(url);
    }

    public Request request() {
        return request;
    }
}
