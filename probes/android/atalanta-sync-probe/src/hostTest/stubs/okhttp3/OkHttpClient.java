package okhttp3;

/** Host-check stand-in for OkHttp's OkHttpClient. */
public final class OkHttpClient {
    private final Dispatcher dispatcher = new Dispatcher();

    public Dispatcher dispatcher() {
        return dispatcher;
    }
}
