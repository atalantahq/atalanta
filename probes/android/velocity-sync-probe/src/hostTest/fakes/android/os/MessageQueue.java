package android.os;

/** Host-check fake with AOSP's {@code mMessages} list, sorted by {@code when}. */
public final class MessageQueue {
    Message mMessages;

    public synchronized void enqueue(Message msg) {
        if (mMessages == null || msg.when < mMessages.when) {
            msg.next = mMessages;
            mMessages = msg;
            return;
        }
        Message p = mMessages;
        while (p.next != null && p.next.when <= msg.when) p = p.next;
        msg.next = p.next;
        p.next = msg;
    }

    /** Remove messages with this callback (Handler.removeCallbacks). */
    public synchronized void remove(Runnable callback) {
        Message prev = null;
        for (Message p = mMessages; p != null; p = p.next) {
            if (p.callback == callback) {
                if (prev == null) mMessages = p.next; else prev.next = p.next;
            } else {
                prev = p;
            }
        }
    }

    /** Run (drop) everything due at or before {@code now}; returns the callbacks that ran. */
    public synchronized java.util.List<Runnable> runDue(long now) {
        java.util.List<Runnable> ran = new java.util.ArrayList<>();
        while (mMessages != null && mMessages.when <= now) {
            if (mMessages.callback != null) ran.add(mMessages.callback);
            mMessages = mMessages.next;
        }
        return ran;
    }

    public synchronized void clear() {
        mMessages = null;
    }

    /** What Looper.dump prints for this queue (Message.toString(now) per message). */
    public synchronized java.util.List<String> dump(long now) {
        java.util.List<String> out = new java.util.ArrayList<>();
        out.add("Looper (main, tid 1) {1234}");
        int i = 0;
        for (Message p = mMessages; p != null; p = p.next) {
            StringBuilder b = new StringBuilder("  Message ").append(i++).append(": { when=");
            long d = p.when - now;
            b.append(d == 0 ? "0" : (d > 0 ? "+" : "-") + format(Math.abs(d)));
            if (p.target == null) {
                b.append(" barrier=0");
            } else {
                if (p.callback != null) b.append(" callback=").append(p.callback.getClass().getName());
                else b.append(" what=").append(p.what);
                b.append(" target=").append(p.target.getClass().getName());
            }
            out.add(b.append(" }").toString());
        }
        out.add("  (Total messages: " + i + ", polling=false, quitting=false)");
        return out;
    }

    private static String format(long ms) {
        StringBuilder b = new StringBuilder();
        if (ms >= 60_000) { b.append(ms / 60_000).append('m'); ms %= 60_000; }
        if (ms >= 1_000) { b.append(ms / 1_000).append('s'); ms %= 1_000; }
        if (ms > 0 || b.length() == 0) b.append(ms).append("ms");
        return b.toString();
    }
}
