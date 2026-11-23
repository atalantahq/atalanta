package okio;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/** Host-check stand-in for Okio's Buffer: a byte FIFO that is also a Source. */
public final class Buffer implements Source {
    private byte[] data = new byte[0];

    public Buffer write(byte[] bytes) {
        byte[] n = Arrays.copyOf(data, data.length + bytes.length);
        System.arraycopy(bytes, 0, n, data.length, bytes.length);
        data = n;
        return this;
    }

    public long size() {
        return data.length;
    }

    @Override
    public long read(Buffer sink, long byteCount) {
        if (data.length == 0) return -1;
        int n = (int) Math.min(byteCount, data.length);
        sink.write(Arrays.copyOf(data, n));
        data = Arrays.copyOfRange(data, n, data.length);
        return n;
    }

    public String readUtf8() {
        String s = new String(data, StandardCharsets.UTF_8);
        data = new byte[0];
        return s;
    }

    @Override
    public Timeout timeout() {
        return Timeout.NONE;
    }

    @Override
    public void close() {}

    static String drain(Source s) throws java.io.IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        Buffer b = new Buffer();
        while (s.read(b, 4) != -1) {
            out.write(b.data, 0, b.data.length);
            b.data = new byte[0];
        }
        return new String(out.toByteArray(), StandardCharsets.UTF_8);
    }
}
