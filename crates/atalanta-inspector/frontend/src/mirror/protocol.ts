// The mirror's wire format and the pure helpers around it (no DOM), so
// they can be unit-tested with `node --test`.
//
// Binary messages from `/api/mirror/{device}`: an 18-byte header
//   kind u8 (1 SPS/PPS, 2 H.264 access unit, 3 JPEG, 4 PNG)
//   flags u8 (bit 0: key frame)
//   pts u64 (µs, the encoder's clock)
//   host receive time u64 (Unix µs)
// then the payload (Annex-B for H.264).

export const KIND_CONFIG = 1;
export const KIND_H264 = 2;
export const KIND_JPEG = 3;
export const KIND_PNG = 4;
export const HEADER_LEN = 18;

export interface MirrorFrame {
  kind: number;
  key: boolean;
  /** Encoder pts, µs. */
  pts: number;
  /** When the host received it, Unix µs. */
  receivedUs: number;
  data: Uint8Array;
}

export function parseFrame(buf: ArrayBuffer): MirrorFrame | null {
  if (buf.byteLength < HEADER_LEN) return null;
  const v = new DataView(buf);
  return {
    kind: v.getUint8(0),
    key: (v.getUint8(1) & 1) === 1,
    pts: Number(v.getBigUint64(2)),
    receivedUs: Number(v.getBigUint64(10)),
    data: new Uint8Array(buf, HEADER_LEN),
  };
}

/** Byte offsets of the NAL units of an Annex-B buffer (after each start code). */
export function nalStarts(b: Uint8Array): number[] {
  const out: number[] = [];
  for (let i = 0; i + 2 < b.length; i++) {
    if (b[i] === 0 && b[i + 1] === 0) {
      if (b[i + 2] === 1) {
        out.push(i + 3);
        i += 2;
      } else if (b[i + 2] === 0 && i + 3 < b.length && b[i + 3] === 1) {
        out.push(i + 4);
        i += 3;
      }
    }
  }
  return out;
}

/** The NAL unit types in an Annex-B buffer. */
export function nalTypes(b: Uint8Array): number[] {
  return nalStarts(b)
    .filter((i) => i < b.length)
    .map((i) => b[i] & 0x1f);
}

/** `avc1.PPCCLL` from the first SPS of an Annex-B buffer. */
export function codecFromConfig(b: Uint8Array): string | null {
  for (const i of nalStarts(b)) {
    if ((b[i] & 0x1f) === 7 && i + 3 < b.length) {
      const hex = (n: number) => n.toString(16).padStart(2, "0");
      return `avc1.${hex(b[i + 1])}${hex(b[i + 2])}${hex(b[i + 3])}`;
    }
  }
  return null;
}

/** A key frame's data with the parameter sets in front, unless it has them.
 *  WebCodecs decodes Annex-B (no `description`) only with SPS/PPS in band. */
export function withConfig(frame: Uint8Array, config: Uint8Array | null): Uint8Array {
  if (!config || nalTypes(frame).includes(7)) return frame;
  const out = new Uint8Array(config.length + frame.length);
  out.set(config, 0);
  out.set(frame, config.length);
  return out;
}

// ── Coordinates and gestures ────────────────────────────────────────────

export interface Size {
  width: number;
  height: number;
}

export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** A point on the shown picture (client coordinates) in device units
 *  (`space`: what taps take), or null when it is outside the picture. */
export function toDevice(
  clientX: number,
  clientY: number,
  shown: Box,
  space: Size,
): { x: number; y: number } | null {
  const fx = (clientX - shown.left) / shown.width;
  const fy = (clientY - shown.top) / shown.height;
  if (!(fx >= 0 && fx <= 1 && fy >= 0 && fy <= 1)) return null;
  return {
    x: Math.min(space.width - 1, Math.max(0, Math.round(fx * space.width))),
    y: Math.min(space.height - 1, Math.max(0, Math.round(fy * space.height))),
  };
}

/** The box a `pic`-sized picture fills inside `box` (letterboxed, like
 *  `object-fit: contain`). */
export function containBox(box: Box, pic: Size): Box {
  if (!pic.width || !pic.height || !box.width || !box.height) return box;
  const scale = Math.min(box.width / pic.width, box.height / pic.height);
  const width = pic.width * scale;
  const height = pic.height * scale;
  return {
    left: box.left + (box.width - width) / 2,
    top: box.top + (box.height - height) / 2,
    width,
    height,
  };
}

export type Gesture =
  | { kind: "tap"; x: number; y: number }
  | { kind: "longPress"; x: number; y: number; ms: number }
  | { kind: "swipe"; from: [number, number]; to: [number, number] };

/** A press from `down` to `up` (device units) lasting `ms`: a tap, a long
 *  press, or a swipe when it moved more than `slop` (device units). */
export function classify(
  down: { x: number; y: number },
  up: { x: number; y: number },
  ms: number,
  slop: number,
): Gesture {
  const moved = Math.hypot(up.x - down.x, up.y - down.y);
  if (moved > slop) return { kind: "swipe", from: [down.x, down.y], to: [up.x, up.y] };
  if (ms >= 600) return { kind: "longPress", x: down.x, y: down.y, ms: Math.min(ms, 5000) };
  return { kind: "tap", x: down.x, y: down.y };
}

/** The key a keyboard event sends, or null for text (or nothing). */
export function keyFor(key: string): string | null {
  switch (key) {
    case "Enter":
      return "enter";
    case "Backspace":
      return "backspace";
    case "Delete":
      return "delete";
    case "Tab":
      return "tab";
    case "Escape":
      return "escape";
    case "ArrowUp":
      return "arrowUp";
    case "ArrowDown":
      return "arrowDown";
    case "ArrowLeft":
      return "arrowLeft";
    case "ArrowRight":
      return "arrowRight";
    default:
      return null;
  }
}

/** Typed characters are sent in batches: a batch goes once typing pauses. */
export class TextBatcher {
  private buf = "";
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly send: (text: string) => void;
  private readonly delayMs: number;

  constructor(send: (text: string) => void, delayMs = 250) {
    this.send = send;
    this.delayMs = delayMs;
  }

  push(text: string) {
    this.buf += text;
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => this.flush(), this.delayMs);
  }

  flush() {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    if (this.buf) {
      const t = this.buf;
      this.buf = "";
      this.send(t);
    }
  }
}

// ── Element hit testing ─────────────────────────────────────────────────

export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface Node {
  bounds: Bounds;
  visible: boolean;
  children: Node[];
}

/** The smallest visible element at (x, y), deepest first on ties. Iterative. */
export function hitTest<T extends Node>(root: T | null, x: number, y: number): T | null {
  if (!root) return null;
  let best: T | null = null;
  let bestArea = Infinity;
  const stack: T[] = [root];
  while (stack.length) {
    const el = stack.pop()!;
    const b = el.bounds;
    if (x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height) {
      const area = b.width * b.height;
      if (el.visible !== false && area > 0 && area <= bestArea) {
        best = el;
        bestArea = area;
      }
      for (const c of el.children as T[]) stack.push(c);
    }
  }
  return best;
}

// ── Stats ───────────────────────────────────────────────────────────────

/** Painted frames per second and host→paint latency over a sliding window. */
export class Stats {
  private paints: number[] = [];
  private lat: number[] = [];
  readonly windowMs: number;

  constructor(windowMs = 2000) {
    this.windowMs = windowMs;
  }

  painted(nowMs: number, latencyMs: number | null) {
    this.paints.push(nowMs);
    if (latencyMs != null && latencyMs >= 0 && latencyMs < 60_000) this.lat.push(latencyMs);
    while (this.paints.length && nowMs - this.paints[0] > this.windowMs) this.paints.shift();
    if (this.lat.length > 120) this.lat.splice(0, this.lat.length - 120);
  }

  fps(nowMs: number): number {
    const recent = this.paints.filter((t) => nowMs - t <= this.windowMs);
    return (recent.length * 1000) / this.windowMs;
  }

  /** Median latency of the recent frames, ms. */
  latency(): number | null {
    if (!this.lat.length) return null;
    const s = [...this.lat].sort((a, b) => a - b);
    return s[Math.floor(s.length / 2)];
  }
}
