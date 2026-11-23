// The live mirror: a WebSocket to /api/mirror/{device}, decoding H.264
// with WebCodecs (or drawing pushed JPEG/PNG frames) onto a canvas.
//
// - No WebCodecs, or H.264 not supported: connects with `?mode=frames`.
// - A decoder error, or a decode queue that keeps growing: the decoder is
//   reset, a key frame is asked for, and deltas are dropped until it comes.
//   Three decoder failures in a row switch the socket to frames.
// - The tab hidden: `pause` (nothing is sent); shown again: `resume` (the
//   server asks the encoder for a key frame).
// - The socket closes: reconnect, 0.5 s doubling to 10 s.
import {
  KIND_CONFIG,
  KIND_H264,
  KIND_JPEG,
  KIND_PNG,
  Stats,
  codecFromConfig,
  parseFrame,
  withConfig,
  type MirrorFrame,
} from "./protocol";

export interface MirrorStatus {
  connected: boolean;
  mode: "h264" | "frames" | "connecting";
  source: string;
  width: number;
  height: number;
  fps: number;
  /** Median host-receive → painted latency, ms. */
  latencyMs: number | null;
  decoded: number;
  dropped: number;
  note: string | null;
  /** The size the server announced for the picture (a web page's CSS
   *  viewport; the video's size on Android). */
  session: { width: number; height: number } | null;
}

type Listener = (s: MirrorStatus) => void;

const MAX_QUEUE = 6;

export function webCodecsAvailable(): boolean {
  return typeof (globalThis as { VideoDecoder?: unknown }).VideoDecoder === "function";
}

export class MirrorClient {
  private ws: WebSocket | null = null;
  private closed = false;
  private retryMs = 500;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private framesOnly: boolean;
  private decoder: VideoDecoder | null = null;
  private codec: string | null = null;
  private config: Uint8Array | null = null;
  private needKey = true;
  private failures = 0;
  private readonly stats = new Stats();
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D;
  private readonly deviceId: string;
  private listeners: Listener[] = [];
  private statusTimer: ReturnType<typeof setInterval> | null = null;
  /** Host receive time (Unix ms) of frames waiting in the decoder, by pts. */
  private inFlight = new Map<number, number>();
  private imageBusy = false;
  status: MirrorStatus = {
    connected: false,
    mode: "connecting",
    source: "",
    width: 0,
    height: 0,
    fps: 0,
    latencyMs: null,
    decoded: 0,
    dropped: 0,
    note: null,
    session: null,
  };

  constructor(deviceId: string, canvas: HTMLCanvasElement) {
    this.deviceId = deviceId;
    this.canvas = canvas;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("no 2D canvas");
    this.ctx = ctx;
    this.framesOnly = !webCodecsAvailable();
    if (this.framesOnly) this.status.note = "WebCodecs unavailable: showing still frames";
  }

  onStatus(fn: Listener) {
    this.listeners.push(fn);
    return () => {
      this.listeners = this.listeners.filter((l) => l !== fn);
    };
  }

  private emit() {
    const now = performance.now();
    this.status = {
      ...this.status,
      fps: Math.round(this.stats.fps(now) * 10) / 10,
      latencyMs: this.stats.latency(),
    };
    // For the headless check and the curious: window.__atalantaMirror.
    (globalThis as Record<string, unknown>).__atalantaMirror = this.status;
    this.listeners.forEach((l) => l(this.status));
  }

  start() {
    document.addEventListener("visibilitychange", this.onVisibility);
    this.statusTimer = setInterval(() => this.emit(), 500);
    this.connect();
  }

  stop() {
    this.closed = true;
    document.removeEventListener("visibilitychange", this.onVisibility);
    if (this.retryTimer) clearTimeout(this.retryTimer);
    if (this.statusTimer) clearInterval(this.statusTimer);
    this.closeDecoder();
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onmessage = null;
      this.ws.close();
    }
    this.ws = null;
    this.listeners = [];
  }

  /** Ask for a key frame (and the latest still). */
  refresh() {
    this.send({ type: "key" });
  }

  private onVisibility = () => {
    this.send({ type: document.hidden ? "pause" : "resume" });
    if (!document.hidden) this.needKey = true;
  };

  private send(msg: object) {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  private connect() {
    if (this.closed) return;
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const mode = this.framesOnly ? "?mode=frames" : "";
    const ws = new WebSocket(
      `${proto}//${location.host}/api/mirror/${encodeURIComponent(this.deviceId)}${mode}`,
    );
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.status = { ...this.status, connected: false, mode: "connecting" };
    ws.onopen = () => {
      this.retryMs = 500;
      this.status = { ...this.status, connected: true };
      if (document.hidden) this.send({ type: "pause" });
      this.emit();
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") this.onText(ev.data);
      else this.onBinary(ev.data as ArrayBuffer);
    };
    ws.onclose = () => {
      this.closeDecoder();
      this.status = { ...this.status, connected: false };
      this.emit();
      if (this.closed) return;
      this.retryTimer = setTimeout(() => this.connect(), this.retryMs);
      this.retryMs = Math.min(this.retryMs * 2, 10_000);
    };
    ws.onerror = () => ws.close();
  }

  private onText(text: string) {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    switch (msg.type) {
      case "hello":
        this.status = {
          ...this.status,
          mode: msg.mode === "h264" ? "h264" : "frames",
          source: String(msg.source ?? ""),
        };
        this.needKey = true;
        break;
      case "session":
        this.status = { ...this.status, session: { width: Number(msg.width), height: Number(msg.height) } };
        this.resize(Number(msg.width), Number(msg.height));
        this.needKey = true;
        break;
      case "config":
        if (typeof msg.codec === "string") this.codec = msg.codec;
        break;
      case "fallback":
        this.closeDecoder();
        this.status = { ...this.status, note: `H.264 unavailable: ${String(msg.reason ?? "")}` };
        break;
      case "lagged":
        this.needKey = true;
        this.status = { ...this.status, dropped: this.status.dropped + 1 };
        break;
    }
    this.emit();
  }

  private resize(w: number, h: number) {
    if (!w || !h) return;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    this.status = { ...this.status, width: w, height: h };
  }

  private onBinary(buf: ArrayBuffer) {
    const f = parseFrame(buf);
    if (!f) return;
    if (f.kind === KIND_CONFIG) {
      this.config = f.data.slice();
      this.codec = this.codec ?? codecFromConfig(this.config);
      this.closeDecoder(); // a new configuration (e.g. after rotation)
    } else if (f.kind === KIND_H264) {
      this.decode(f);
    } else if (f.kind === KIND_JPEG || f.kind === KIND_PNG) {
      void this.drawImage(f);
    }
  }

  private ensureDecoder(): VideoDecoder | null {
    if (this.decoder && this.decoder.state !== "closed") return this.decoder;
    const codec = this.codec ?? (this.config && codecFromConfig(this.config));
    if (!codec) return null;
    try {
      const d = new VideoDecoder({
        output: (frame) => this.paint(frame),
        error: (e) => this.decoderFailed(String(e)),
      });
      d.configure({ codec, optimizeForLatency: true });
      this.decoder = d;
      return d;
    } catch (e) {
      this.decoderFailed(String(e));
      return null;
    }
  }

  private decode(f: MirrorFrame) {
    if (this.needKey && !f.key) {
      this.status.dropped++;
      return;
    }
    const d = this.ensureDecoder();
    if (!d) return;
    if (d.decodeQueueSize > MAX_QUEUE) {
      // Falling behind: drop to the next key frame.
      this.needKey = true;
      this.status.dropped++;
      this.send({ type: "key" });
      return;
    }
    this.needKey = false;
    const data = f.key ? withConfig(f.data, this.config) : f.data;
    // 0: replayed from the server's buffer, not a live frame.
    if (f.receivedUs > 0) this.inFlight.set(f.pts, f.receivedUs / 1000);
    if (this.inFlight.size > 64) {
      const first = this.inFlight.keys().next().value;
      if (first !== undefined) this.inFlight.delete(first);
    }
    try {
      d.decode(new EncodedVideoChunk({ type: f.key ? "key" : "delta", timestamp: f.pts, data }));
    } catch (e) {
      this.decoderFailed(String(e));
    }
  }

  private paint(frame: VideoFrame) {
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    this.resize(w, h);
    this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
    const recv = this.inFlight.get(frame.timestamp);
    this.inFlight.delete(frame.timestamp);
    frame.close();
    this.failures = 0;
    this.status.decoded++;
    this.stats.painted(performance.now(), recv != null ? Date.now() - recv : null);
  }

  private decoderFailed(why: string) {
    this.closeDecoder();
    this.needKey = true;
    this.failures++;
    this.send({ type: "key" });
    if (this.failures >= 3 && !this.framesOnly) {
      // This browser can't decode the stream: still frames instead.
      this.framesOnly = true;
      this.status = { ...this.status, note: `H.264 decode failed (${why}): showing still frames` };
      this.ws?.close();
    }
  }

  private closeDecoder() {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        // already closed
      }
    }
    this.decoder = null;
    this.inFlight.clear();
  }

  private async drawImage(f: MirrorFrame) {
    // Paint the newest only: a frame arriving while one decodes is dropped.
    if (this.imageBusy) {
      this.status.dropped++;
      return;
    }
    this.imageBusy = true;
    try {
      const blob = new Blob([f.data as BlobPart], {
        type: f.kind === KIND_PNG ? "image/png" : "image/jpeg",
      });
      const bmp = await createImageBitmap(blob);
      this.resize(bmp.width, bmp.height);
      this.ctx.drawImage(bmp, 0, 0, this.canvas.width, this.canvas.height);
      bmp.close();
      this.status.decoded++;
      this.stats.painted(performance.now(), Date.now() - f.receivedUs / 1000);
    } catch {
      this.status.dropped++;
    } finally {
      this.imageBusy = false;
    }
  }
}
