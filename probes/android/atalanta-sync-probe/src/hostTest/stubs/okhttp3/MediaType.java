package okhttp3;

/** Host-check stand-in for OkHttp's MediaType. */
public final class MediaType {
    private final String value;

    public MediaType(String value) {
        this.value = value;
    }

    @Override
    public String toString() {
        return value;
    }
}
