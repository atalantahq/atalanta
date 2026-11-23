// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okhttp3;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/** Host-check stand-in for OkHttp's Dispatcher (snapshots, as OkHttp returns). */
public final class Dispatcher {
    public final List<Call> running = Collections.synchronizedList(new ArrayList<>());
    public final List<Call> queued = Collections.synchronizedList(new ArrayList<>());

    public List<Call> runningCalls() {
        return new ArrayList<>(running);
    }

    public List<Call> queuedCalls() {
        return new ArrayList<>(queued);
    }
}
