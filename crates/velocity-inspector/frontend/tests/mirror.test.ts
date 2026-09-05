// Run: node --test tests/*.test.ts  (Node >= 22.18 strips the TypeScript types)
import { test } from "node:test";
import assert from "node:assert/strict";

const p = await import("../src/mirror/protocol.ts");
const { summarize } = await import("../src/tasks.ts");

function header(kind: number, key: boolean, pts: bigint, recv: bigint, data: number[]): ArrayBuffer {
  const buf = new ArrayBuffer(18 + data.length);
  const v = new DataView(buf);
  v.setUint8(0, kind);
  v.setUint8(1, key ? 1 : 0);
  v.setBigUint64(2, pts);
  v.setBigUint64(10, recv);
  new Uint8Array(buf, 18).set(data);
  return buf;
}

test("frames parse with the server's 18-byte header", () => {
  const f = p.parseFrame(header(p.KIND_H264, true, 123456n, 1761360000123456n, [0, 0, 0, 1, 0x65]))!;
  assert.equal(f.kind, p.KIND_H264);
  assert.equal(f.key, true);
  assert.equal(f.pts, 123456);
  assert.equal(f.receivedUs, 1761360000123456);
  assert.deepEqual([...f.data], [0, 0, 0, 1, 0x65]);
  assert.equal(p.parseFrame(new ArrayBuffer(5)), null);
});

test("the codec string comes from the SPS; key frames get SPS/PPS in band", () => {
  const config = new Uint8Array([0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0x8d, 0, 0, 0, 1, 0x68, 0xce]);
  assert.equal(p.codecFromConfig(config), "avc1.42c01f");
  assert.deepEqual(p.nalTypes(config), [7, 8]);
  const idr = new Uint8Array([0, 0, 1, 0x65, 0x88]);
  const joined = p.withConfig(idr, config);
  assert.deepEqual(p.nalTypes(joined), [7, 8, 5]);
  // Already carrying them: unchanged.
  assert.equal(p.withConfig(joined, config), joined);
  assert.equal(p.withConfig(idr, null), idr);
});

test("clicks map to device coordinates, outside the picture to nothing", () => {
  const shown = { left: 100, top: 50, width: 360, height: 800 };
  const space = { width: 1080, height: 2400 };
  assert.deepEqual(p.toDevice(100 + 180, 50 + 400, shown, space), { x: 540, y: 1200 });
  assert.deepEqual(p.toDevice(100, 50, shown, space), { x: 0, y: 0 });
  assert.deepEqual(p.toDevice(460, 850, shown, space), { x: 1079, y: 2399 });
  assert.equal(p.toDevice(99, 60, shown, space), null);
  // Letterboxing: a 1:2 picture in a square box is centred.
  assert.deepEqual(p.containBox({ left: 0, top: 0, width: 400, height: 400 }, { width: 100, height: 200 }), {
    left: 100,
    top: 0,
    width: 200,
    height: 400,
  });
});

test("gestures: tap, long press, swipe", () => {
  assert.deepEqual(p.classify({ x: 10, y: 10 }, { x: 12, y: 11 }, 80, 20), { kind: "tap", x: 10, y: 10 });
  assert.equal(p.classify({ x: 10, y: 10 }, { x: 10, y: 10 }, 900, 20).kind, "longPress");
  assert.deepEqual(p.classify({ x: 500, y: 1800 }, { x: 500, y: 600 }, 300, 20), {
    kind: "swipe",
    from: [500, 1800],
    to: [500, 600],
  });
  assert.equal(p.keyFor("Enter"), "enter");
  assert.equal(p.keyFor("ArrowLeft"), "arrowLeft");
  assert.equal(p.keyFor("a"), null);
});

test("typing is batched until it pauses", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const sent: string[] = [];
  const b = new p.TextBatcher((s: string) => sent.push(s), 250);
  b.push("h");
  b.push("i");
  t.mock.timers.tick(200);
  assert.deepEqual(sent, []);
  t.mock.timers.tick(100);
  assert.deepEqual(sent, ["hi"]);
  b.push("!");
  b.flush();
  assert.deepEqual(sent, ["hi", "!"]);
});

test("hit testing finds the smallest visible element under the point", () => {
  const el = (x: number, y: number, w: number, h: number, children: unknown[] = [], visible = true) => ({
    bounds: { x, y, width: w, height: h },
    visible,
    children,
  });
  const button = el(10, 10, 100, 40);
  const hidden = el(10, 10, 20, 20, [], false);
  const root = el(0, 0, 1080, 2400, [el(0, 0, 500, 500, [button, hidden])]);
  assert.equal(p.hitTest(root, 50, 30), button);
  assert.equal(p.hitTest(root, 2000, 30), null);
});

test("stats: fps over the window and median latency", () => {
  const s = new p.Stats(1000);
  for (let i = 0; i < 30; i++) s.painted(i * 33, 20 + (i % 3));
  assert.ok(Math.abs(s.fps(990) - 30) <= 1, String(s.fps(990)));
  assert.equal(s.latency(), 21);
});

test("task events fold into steps, explore lines and status", () => {
  const ev = (seq: number, kind: string, data: Record<string, unknown>) => ({ seq, t: seq, kind, data });
  const s = summarize([
    ev(0, "status", { status: "queued" }),
    ev(1, "status", { status: "running" }),
    ev(2, "testStarted", { test: "login" }),
    ev(3, "stepStarted", { test: "login", index: 0, step: "launchApp" }),
    ev(4, "stepFinished", { test: "login", index: 0, step: "launchApp", status: "passed", ms: 812 }),
    ev(5, "stepStarted", { test: "login", index: 1, step: "tap { text: \"Pay\" }" }),
    ev(6, "log", { stream: "stdout", line: "✗ login" }),
    ev(7, "stepFinished", { test: "login", index: 1, step: "tap", status: "failed", ms: 5000, error: "not found" }),
    ev(8, "explore", { event: "observed", turn: 1, title: "Home" }),
    ev(9, "status", { status: "failed", summary: "0 passed, 1 failed of 1" }),
  ]);
  assert.equal(s.status, "failed");
  assert.equal(s.summary, "0 passed, 1 failed of 1");
  assert.equal(s.steps.length, 2);
  assert.equal(s.steps[0].ms, 812);
  assert.equal(s.steps[1].status, "failed");
  assert.equal(s.steps[1].error, "not found");
  assert.equal(s.explore.length, 1);
  assert.deepEqual(s.log, ["✗ login"]);
});

test("an explore task's result and save fold into the summary", () => {
  const ev = (seq: number, kind: string, data: Record<string, unknown>) => ({ seq, t: seq, kind, data });
  const s = summarize([
    ev(0, "status", { status: "running" }),
    ev(1, "explore", { event: "planned", turn: 1, op: "tap", target: "Network & internet" }),
    ev(2, "instruction", { text: "use search" }),
    ev(3, "stepStarted", { test: "open network", index: 0, total: 2, step: "launchApp" }),
    ev(4, "result", {
      outcome: "success",
      reason: "the page is open",
      yaml: "appId: com.android.settings\n",
      file: "open-network-internet.yaml",
      replay: [{ passed: true, ms: 1300, failedStep: null, error: null }],
    }),
    ev(5, "status", { status: "passed", summary: "success: the page is open; replay 1/1 passed" }),
    ev(6, "saved", { path: "flows/open-network.yaml" }),
  ]);
  assert.equal(s.status, "passed");
  assert.equal(s.explore.length, 2);
  assert.equal(s.steps.length, 1, "the replay's steps fill the table");
  assert.equal(s.result?.file, "open-network-internet.yaml");
  assert.equal(s.result?.replay[0].passed, true);
  assert.equal(s.saved, "flows/open-network.yaml");
});
