/*
 * Velocity screen video server.
 *
 * The capture and encoding design (a virtual display or SurfaceControl
 * display mirroring the screen onto a MediaCodec input surface, the encoder
 * format keys, the size fallbacks, the 12-byte packet header with its
 * config/key-frame flags, the session packet on a size change) is adapted
 * from scrcpy's server (https://github.com/Genymobile/scrcpy,
 * server/src/main/java/com/genymobile/scrcpy/: video/SurfaceEncoder.java,
 * video/ScreenCapture.java, device/Streamer.java, wrappers/DisplayManager.java
 * and wrappers/SurfaceControl.java).
 *
 *   Copyright (C) 2018 Genymobile
 *   Copyright (C) 2018-2025 Romain Vimont
 *
 *   Licensed under the Apache License, Version 2.0 (the "License");
 *   you may not use this file except in compliance with the License.
 *   You may obtain a copy of the License at
 *
 *       http://www.apache.org/licenses/LICENSE-2.0
 *
 *   Unless required by applicable law or agreed to in writing, software
 *   distributed under the License is distributed on an "AS IS" BASIS,
 *   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *   See the License for the specific language governing permissions and
 *   limitations under the License.
 *
 * Modified for Velocity: one client per process, H.264 only, no audio,
 * control or OpenGL filters, a key-frame request from the host, and the
 * process exits when its client goes.
 */
package com.velocity.tree;

import android.graphics.Rect;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.Build;
import android.os.Bundle;
import android.os.IBinder;
import android.os.Looper;
import android.view.Surface;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;

/**
 * Streams the screen as H.264 to one host client, scrcpy-style.
 *
 * Started by the host with
 *   CLASSPATH=velocity-tree.jar app_process / com.velocity.tree.VideoServer
 *       [--max-size N] [--bit-rate N] [--max-fps N] [--iframe S] [--socket NAME]
 * it prints "velocity-video <version> listening on @NAME", accepts one
 * connection on that abstract socket (adb forward), and streams until the
 * client closes it or sends 'Q'; then the process exits. No client within
 * {@link #ACCEPT_MS} also exits, so a host that died never leaves an
 * encoder running.
 *
 * Stream (all integers big-endian):
 *   u32 codec id ("h264")
 *   packets, each with a 12-byte header:
 *     session: u32 (bit 31 set) flags, u32 width, u32 height, no payload;
 *              sent before the first frame and on every size change
 *              (a rotation). The next packet is a config packet.
 *     media:   u64 pts|flags, u32 size, then size bytes of Annex-B H.264.
 *              bit 62 = config (SPS/PPS, no pts), bit 61 = key frame,
 *              low bits = presentation time in microseconds.
 * Host to device, one byte each: 'K' asks for a key frame now, 'Q' stops.
 */
public final class VideoServer {
    static final String VERSION = "1";
    static final String SOCKET = "velocity_video";
    static final int CODEC_H264 = 0x68323634;
    static final long FLAG_SESSION = 1L << 63;
    static final long FLAG_CONFIG = 1L << 62;
    static final long FLAG_KEY_FRAME = 1L << 61;
    /** Exit when no client connects within this long. */
    static final long ACCEPT_MS = 15000;
    /** Keep the values in descending order (scrcpy's MAX_SIZE_FALLBACK). */
    static final int[] MAX_SIZE_FALLBACK = {1920, 1600, 1280, 1024, 800};
    /** Repeat the last frame after 100 ms without a new one, as scrcpy does. */
    static final long REPEAT_FRAME_DELAY_US = 100_000;

    private static volatile boolean stopped;
    private static volatile boolean syncWanted;
    private static volatile MediaCodec running;

    public static void main(String[] args) throws Exception {
        int maxSize = 1280;
        int bitRate = 2_000_000;
        float maxFps = 30;
        int iframe = 1;
        String socket = SOCKET;
        for (int i = 0; i + 1 < args.length; i += 2) {
            switch (args[i]) {
                case "--max-size": maxSize = Integer.parseInt(args[i + 1]); break;
                case "--bit-rate": bitRate = Integer.parseInt(args[i + 1]); break;
                case "--max-fps": maxFps = Float.parseFloat(args[i + 1]); break;
                case "--iframe": iframe = Integer.parseInt(args[i + 1]); break;
                case "--socket": socket = args[i + 1]; break;
                default: break;
            }
        }
        LocalServerSocket server = new LocalServerSocket(socket);
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(ACCEPT_MS);
            } catch (InterruptedException e) {
                return;
            }
            System.err.println("velocity-video: no client in " + ACCEPT_MS + " ms");
            System.exit(2);
        });
        watchdog.setDaemon(true);
        watchdog.start();
        System.out.println("velocity-video " + VERSION + " listening on @" + socket);
        System.out.flush();
        LocalSocket client = server.accept();
        watchdog.interrupt();
        // The abstract socket name is public (see Server.serve): adb only.
        int uid = client.getPeerCredentials().getUid();
        if (uid % 100000 >= 10000) {
            client.close();
            System.exit(3);
        }
        server.close();
        final InputStream in = client.getInputStream();
        Thread control = new Thread(() -> {
            try {
                int c;
                while ((c = in.read()) != -1) {
                    if (c == 'K') {
                        syncWanted = true;
                    } else if (c == 'Q') {
                        break;
                    }
                }
            } catch (IOException ignored) {
                // client gone
            }
            stop();
        });
        control.setDaemon(true);
        control.start();

        // Some devices deadlock if the encoding thread has no Looper (scrcpy #4143).
        Looper.prepare();
        int code = 0;
        try {
            stream(client.getOutputStream(), maxSize, bitRate, maxFps, iframe);
        } catch (Throwable e) {
            if (!stopped) {
                System.err.println("velocity-video: " + e);
                code = 1;
            }
        }
        try {
            client.close();
        } catch (IOException ignored) {
            // closing anyway
        }
        System.exit(code);
    }

    private static void stop() {
        stopped = true;
        MediaCodec c = running;
        if (c != null) {
            try {
                c.signalEndOfInputStream();
            } catch (Exception ignored) {
                // not running any more
            }
        }
    }

    /** Display size and rotation, read through DisplayManagerGlobal. */
    static final class Display {
        final int width;
        final int height;
        final int rotation;
        final int layerStack;

        Display(int width, int height, int rotation, int layerStack) {
            this.width = width;
            this.height = height;
            this.rotation = rotation;
            this.layerStack = layerStack;
        }

        boolean sameAs(Display o) {
            return o != null && o.width == width && o.height == height && o.rotation == rotation;
        }
    }

    static Display display() throws Exception {
        Class<?> cls = Class.forName("android.hardware.display.DisplayManagerGlobal");
        Object dmg = cls.getDeclaredMethod("getInstance").invoke(null);
        Object info = dmg.getClass().getMethod("getDisplayInfo", int.class).invoke(dmg, 0);
        Class<?> ic = info.getClass();
        return new Display(
                ic.getDeclaredField("logicalWidth").getInt(info),
                ic.getDeclaredField("logicalHeight").getInt(info),
                ic.getDeclaredField("rotation").getInt(info),
                ic.getDeclaredField("layerStack").getInt(info));
    }

    /**
     * The video size: the display scaled so its long edge is at most
     * {@code maxSize}, both sides a multiple of 8 (encoders want 16 or 8;
     * scrcpy rounds to 8).
     */
    static int[] videoSize(int w, int h, int maxSize) {
        int longEdge = Math.max(w, h);
        if (maxSize > 0 && longEdge > maxSize) {
            if (w >= h) {
                h = h * maxSize / w;
                w = maxSize;
            } else {
                w = w * maxSize / h;
                h = maxSize;
            }
        }
        return new int[] {Math.max(8, w & ~7), Math.max(8, h & ~7)};
    }

    static MediaFormat format(int bitRate, float maxFps, int iframe) {
        MediaFormat f = new MediaFormat();
        f.setString(MediaFormat.KEY_MIME, MediaFormat.MIMETYPE_VIDEO_AVC);
        f.setInteger(MediaFormat.KEY_BIT_RATE, bitRate);
        // Required, but the rate is variable with a surface input.
        f.setInteger(MediaFormat.KEY_FRAME_RATE, 60);
        f.setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
        if (Build.VERSION.SDK_INT >= 24) {
            f.setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED);
        }
        f.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, iframe);
        // The first frame shows, and a still screen still yields frames.
        f.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, REPEAT_FRAME_DELAY_US);
        if (Build.VERSION.SDK_INT >= 23) {
            f.setInteger(MediaFormat.KEY_PRIORITY, 0);
        }
        if (Build.VERSION.SDK_INT >= 26) {
            f.setInteger(MediaFormat.KEY_LATENCY, 1);
        }
        if (maxFps > 0) {
            // Private before Android 10, honoured anyway (scrcpy #488).
            f.setFloat("max-fps-to-encoder", maxFps);
        }
        return f;
    }

    private static void stream(OutputStream out, int maxSize, int bitRate, float maxFps, int iframe)
            throws Exception {
        writeInt(out, CODEC_H264);
        out.flush();
        MediaCodec codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
        System.err.println("velocity-video: encoder " + codec.getName());
        MediaFormat format = format(bitRate, maxFps, iframe);
        int size = maxSize;
        boolean sentFrame = false;
        try {
            while (!stopped) {
                Display d = display();
                int[] wh = videoSize(d.width, d.height, size);
                format.setInteger(MediaFormat.KEY_WIDTH, wh[0]);
                format.setInteger(MediaFormat.KEY_HEIGHT, wh[1]);
                Surface surface = null;
                VirtualDisplay vd = null;
                IBinder token = null;
                boolean started = false;
                try {
                    codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
                    surface = codec.createInputSurface();
                    Object shown = mirror(d, wh[0], wh[1], surface);
                    if (shown instanceof VirtualDisplay) {
                        vd = (VirtualDisplay) shown;
                    } else {
                        token = (IBinder) shown;
                    }
                    codec.start();
                    started = true;
                    running = codec;
                    if (stopped) break;
                    writeSession(out, wh[0], wh[1]);
                    sentFrame |= drain(codec, out, d, iframe);
                } catch (IllegalStateException | IllegalArgumentException e) {
                    if (stopped || sentFrame) throw e;
                    // Encoders refuse sizes they can't do: retry smaller,
                    // as scrcpy's downsize-on-error does.
                    int next = 0;
                    for (int v : MAX_SIZE_FALLBACK) {
                        if (v < Math.max(wh[0], wh[1])) {
                            next = v;
                            break;
                        }
                    }
                    if (next == 0) throw e;
                    System.err.println("velocity-video: " + e + "; retrying at " + next);
                    size = next;
                } finally {
                    running = null;
                    if (started) {
                        try {
                            codec.stop();
                        } catch (IllegalStateException ignored) {
                            // already stopped
                        }
                    }
                    codec.reset();
                    if (vd != null) vd.release();
                    if (token != null) destroyDisplay(token);
                    if (surface != null) surface.release();
                }
            }
        } finally {
            codec.release();
        }
    }

    /**
     * Encoded packets to the client until end of stream (stop, or the
     * display changed size). Returns whether a frame was sent.
     */
    private static boolean drain(MediaCodec codec, OutputStream out, Display shown, int iframe)
            throws Exception {
        MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
        boolean sent = false;
        long lastCheck = System.nanoTime();
        // Encoders count the key-frame interval in frames at the nominal
        // rate, and a still screen yields a few frames a second: a key
        // frame came every 20 s. The host's ring buffer cuts at key frames,
        // so ask for one every `iframe` seconds of wall time.
        long lastKey = System.nanoTime();
        long keyEveryNs = Math.max(1, iframe) * 1_000_000_000L;
        byte[] chunk = new byte[64 * 1024];
        while (true) {
            if (syncWanted || System.nanoTime() - lastKey > keyEveryNs) {
                syncWanted = false;
                lastKey = System.nanoTime();
                Bundle b = new Bundle();
                b.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0);
                codec.setParameters(b);
            }
            if (System.nanoTime() - lastCheck > 500_000_000L) {
                lastCheck = System.nanoTime();
                if (!shown.sameAs(display())) {
                    // Rotated or resized: restart at the new size.
                    codec.signalEndOfInputStream();
                }
            }
            int id = codec.dequeueOutputBuffer(info, 100_000);
            if (id < 0) {
                if (stopped) return sent;
                continue;
            }
            try {
                boolean eos = (info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) != 0;
                if (info.size > 0) {
                    boolean config = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0;
                    boolean key = (info.flags & MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0;
                    ByteBuffer buf = codec.getOutputBuffer(id);
                    buf.position(info.offset);
                    buf.limit(info.offset + info.size);
                    long head = config ? FLAG_CONFIG
                            : (info.presentationTimeUs & ~(7L << 61)) | (key ? FLAG_KEY_FRAME : 0);
                    writeLong(out, head);
                    writeInt(out, info.size);
                    while (buf.hasRemaining()) {
                        int n = Math.min(chunk.length, buf.remaining());
                        buf.get(chunk, 0, n);
                        out.write(chunk, 0, n);
                    }
                    out.flush();
                    sent |= !config;
                    if (key) lastKey = System.nanoTime();
                }
                if (eos) return sent;
            } finally {
                codec.releaseOutputBuffer(id, false);
            }
        }
    }

    /**
     * Mirror display 0 onto {@code surface} at w x h: DisplayManager's
     * hidden static createVirtualDisplay (Android 14+), else a
     * SurfaceControl display (up to Android 13), as scrcpy's ScreenCapture.
     */
    private static Object mirror(Display d, int w, int h, Surface surface) throws Exception {
        try {
            Method m = android.hardware.display.DisplayManager.class.getMethod(
                    "createVirtualDisplay", String.class, int.class, int.class, int.class, Surface.class);
            return m.invoke(null, "velocity", w, h, 0, surface);
        } catch (Exception displayManager) {
            Class<?> sc = Class.forName("android.view.SurfaceControl");
            // Shell can't create secure displays since Android 12.
            boolean secure = Build.VERSION.SDK_INT < 30
                    || (Build.VERSION.SDK_INT == 30 && !"S".equals(Build.VERSION.CODENAME));
            IBinder token = (IBinder) sc.getMethod("createDisplay", String.class, boolean.class)
                    .invoke(null, "velocity", secure);
            sc.getMethod("openTransaction").invoke(null);
            try {
                sc.getMethod("setDisplaySurface", IBinder.class, Surface.class).invoke(null, token, surface);
                sc.getMethod("setDisplayProjection", IBinder.class, int.class, Rect.class, Rect.class)
                        .invoke(null, token, 0, new Rect(0, 0, d.width, d.height), new Rect(0, 0, w, h));
                sc.getMethod("setDisplayLayerStack", IBinder.class, int.class).invoke(null, token, d.layerStack);
            } finally {
                sc.getMethod("closeTransaction").invoke(null);
            }
            return token;
        }
    }

    private static void destroyDisplay(IBinder token) {
        try {
            Class.forName("android.view.SurfaceControl").getMethod("destroyDisplay", IBinder.class)
                    .invoke(null, token);
        } catch (Exception ignored) {
            // gone with the process anyway
        }
    }

    private static void writeSession(OutputStream out, int w, int h) throws IOException {
        writeInt(out, (int) (FLAG_SESSION >>> 32));
        writeInt(out, w);
        writeInt(out, h);
        out.flush();
    }

    static void writeInt(OutputStream out, int v) throws IOException {
        out.write(new byte[] {(byte) (v >>> 24), (byte) (v >>> 16), (byte) (v >>> 8), (byte) v});
    }

    static void writeLong(OutputStream out, long v) throws IOException {
        writeInt(out, (int) (v >>> 32));
        writeInt(out, (int) v);
    }
}
