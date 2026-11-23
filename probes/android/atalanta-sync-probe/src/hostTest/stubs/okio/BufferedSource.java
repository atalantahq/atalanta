package okio;

import java.io.IOException;

/** Host-check stand-in for Okio's BufferedSource (only what the checks use). */
public interface BufferedSource extends Source {
    /** Everything to the end, as UTF-8. */
    String readUtf8() throws IOException;
}
