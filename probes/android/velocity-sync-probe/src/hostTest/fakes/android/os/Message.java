package android.os;

/**
 * Host-check fake with AOSP's field names (Message.java): the probe walks
 * {@code when}, {@code target}, {@code callback}, {@code what} and {@code next}
 * by reflection, so the real code path runs. Shadows android.jar's stub on
 * the host-check classpath only.
 */
public final class Message {
    public int what;
    long when;
    Object target;
    Runnable callback;
    Message next;

    public static Message of(long when, Object target, Runnable callback, int what) {
        Message m = new Message();
        m.when = when;
        m.target = target;
        m.callback = callback;
        m.what = what;
        return m;
    }
}
