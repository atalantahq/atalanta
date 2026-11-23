// Run: node --test tests/*.test.ts  (Node >= 22.18 strips the TypeScript types)
import { test } from "node:test";
import assert from "node:assert/strict";

type Handler = ((ev?: unknown) => void) | null;

class FakeWebSocket {
  static OPEN = 1;
  static all: FakeWebSocket[] = [];
  readyState = 1;
  onmessage: Handler = null;
  onclose: Handler = null;
  onerror: Handler = null;
  url: string;
  constructor(url: string) {
    this.url = url;
    FakeWebSocket.all.push(this);
  }
  send() {}
  close() {
    if (this.readyState === 3) return;
    this.readyState = 3;
    // Browsers fire close asynchronously.
    setTimeout(() => this.onclose?.(), 0);
  }
}

const g = globalThis as Record<string, unknown>;
g.WebSocket = FakeWebSocket;
g.location = { protocol: "http:", host: "localhost:9876" };

const { InspectorSocket } = await import("../src/api/websocket.ts");

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const open = () => FakeWebSocket.all.filter((s) => s.readyState !== 3).length;

test("switching devices leaves one open socket (VEL-115)", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let sock = new InspectorSocket("dev-0");
  sock.connect();
  for (let i = 1; i <= 3; i++) {
    sock.disconnect();
    sock = new InspectorSocket(`dev-${i}`);
    sock.connect();
  }
  // Let any close handlers and reconnect timers fire.
  t.mock.timers.tick(10_000);
  t.mock.timers.tick(10_000);
  t.mock.timers.reset();
  await sleep(0);
  assert.equal(open(), 1, `open sockets: ${open()}`);
  assert.equal(FakeWebSocket.all.length, 4, "no reconnects of closed sockets");
  sock.disconnect();
});

test("an unexpected close still reconnects", async (t) => {
  FakeWebSocket.all = [];
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const sock = new InspectorSocket("a/b");
  sock.connect();
  assert.equal(FakeWebSocket.all[0].url, "ws://localhost:9876/api/ws/a%2Fb?screenshots=0");
  FakeWebSocket.all[0].close();
  t.mock.timers.tick(1);
  t.mock.timers.tick(2000);
  assert.equal(FakeWebSocket.all.length, 2);
  sock.disconnect();
  t.mock.timers.tick(5000);
  assert.equal(FakeWebSocket.all.length, 2);
});
