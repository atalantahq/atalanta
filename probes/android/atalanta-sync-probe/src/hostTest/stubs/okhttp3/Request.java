package okhttp3;

/** Host-check stand-in for OkHttp's Request: {@code url()} as OkHttp 3/4 expose it to Java. */
public final class Request {
    private final HttpUrl url;

    public Request(String url) {
        this.url = new HttpUrl(url);
    }

    public HttpUrl url() {
        return url;
    }
}
