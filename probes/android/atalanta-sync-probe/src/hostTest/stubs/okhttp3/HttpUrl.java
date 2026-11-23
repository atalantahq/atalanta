package okhttp3;

/** Host-check stand-in for OkHttp's HttpUrl (only what the probe touches). */
public final class HttpUrl {
    private final String url;

    public HttpUrl(String url) {
        this.url = url;
    }

    @Override
    public String toString() {
        return url;
    }
}
