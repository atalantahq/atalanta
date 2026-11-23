package okio;

import java.io.IOException;

/** Host-check stand-in for Okio's ForwardingSource. */
public abstract class ForwardingSource implements Source {
    private final Source delegate;

    public ForwardingSource(Source delegate) {
        this.delegate = delegate;
    }

    public final Source delegate() {
        return delegate;
    }

    @Override
    public long read(Buffer sink, long byteCount) throws IOException {
        return delegate.read(sink, byteCount);
    }

    @Override
    public Timeout timeout() {
        return delegate.timeout();
    }

    @Override
    public void close() throws IOException {
        delegate.close();
    }
}
