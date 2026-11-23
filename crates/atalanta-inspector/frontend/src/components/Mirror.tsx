// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as consoleApi from "../api/console";
import type { ControlStatus, InputMsg } from "../api/console";
import type { Element } from "../api/types";
import { MirrorClient, type MirrorStatus } from "../mirror/MirrorClient";
import { TextBatcher, classify, hitTest, keyFor, toDevice, type Size } from "../mirror/protocol";
import { useInspectorStore } from "../store/inspectorStore";

type Mode = "interact" | "inspect";

function elementLabel(el: Element): string {
  const name = el.label || el.text || el.platform_id || "";
  const type = el.element_type.split(".").pop() ?? el.element_type;
  return name ? `${type} "${name.length > 40 ? name.slice(0, 37) + "…" : name}"` : type;
}

/** The live device screen: decoded video or pushed frames, with input,
 *  element hover/selection from the latest hierarchy, and device control. */
export function Mirror() {
  const deviceId = useInspectorStore((s) => s.currentDeviceId);
  const hierarchy = useInspectorStore((s) => s.hierarchy);
  const selected = useInspectorStore((s) => s.selectedElement);
  const setSelected = useInspectorStore((s) => s.setSelectedElement);
  const generated = useInspectorStore((s) => s.generated);
  const setError = useInspectorStore((s) => s.setError);

  const canvasRef = useRef<HTMLCanvasElement>(null);
  const clientRef = useRef<MirrorClient | null>(null);
  const [status, setStatus] = useState<MirrorStatus | null>(null);
  const [control, setControl] = useState<ControlStatus | null>(null);
  const [mode, setMode] = useState<Mode>("interact");
  const [hover, setHover] = useState<Element | null>(null);
  const [address, setAddress] = useState("https://");
  const isWeb = deviceId === "web" || (deviceId?.startsWith("web-") ?? false);
  const press = useRef<{ x: number; y: number; t: number } | null>(null);
  const wheel = useRef<{ dy: number; dx: number; x: number; y: number; timer: number | null }>({
    dy: 0,
    dx: 0,
    x: 0,
    y: 0,
    timer: null,
  });

  // The live picture.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!deviceId || !canvas) return;
    const c = new MirrorClient(deviceId, canvas);
    clientRef.current = c;
    const off = c.onStatus(setStatus);
    c.start();
    return () => {
      off();
      c.stop();
      clientRef.current = null;
    };
  }, [deviceId]);

  // Who controls the device; refreshed every few seconds.
  const refreshControl = useCallback(async () => {
    if (!deviceId) return;
    try {
      setControl(await consoleApi.controlStatus(deviceId));
    } catch {
      setControl(null);
    }
  }, [deviceId]);
  useEffect(() => {
    if (!deviceId) return;
    let alive = true;
    const tick = () => {
      if (alive && !document.hidden) void refreshControl();
    };
    tick();
    const t = setInterval(tick, 4000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [deviceId, refreshControl]);

  // Element bounds and taps share the screen's coordinates (device px on
  // Android, points on iOS, CSS px on the web); the root's bounds stand in
  // until the server has said the screen's size.
  // A web page's screencast says which CSS viewport it shows: that, not
  // the configured window, is what its pixels cover.
  const session = isWeb ? status?.session : null;
  const treeSpace: Size | null = useMemo(() => {
    if (session && session.width > 0) return session;
    if (control?.screen) return control.screen;
    const b = hierarchy?.bounds;
    if (b && b.width > 0 && b.height > 0) return { width: b.x + b.width, height: b.y + b.height };
    return null;
  }, [hierarchy, control, session]);
  const tapSpace: Size | null = treeSpace;

  const send = useCallback(
    async (msg: InputMsg) => {
      if (!deviceId) return;
      try {
        await consoleApi.sendInput(deviceId, msg);
        if (!control?.mine) void refreshControl();
      } catch (e: unknown) {
        const m = e instanceof Error ? e.message : String(e);
        setError(m.startsWith("409") ? `Device ${m.slice(5)}` : m);
        void refreshControl();
      }
    },
    [deviceId, control, refreshControl, setError],
  );

  const batcher = useMemo(
    () => new TextBatcher((text) => void send({ kind: "text", text })),
    [send],
  );

  const at = (e: { clientX: number; clientY: number }, space: Size | null) => {
    const canvas = canvasRef.current;
    if (!canvas || !space) return null;
    const r = canvas.getBoundingClientRect();
    return toDevice(e.clientX, e.clientY, r, space);
  };

  const onMove = (e: React.PointerEvent) => {
    const p = at(e, treeSpace);
    setHover(p ? hitTest(hierarchy, p.x, p.y) : null);
  };

  const onDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    e.currentTarget.focus();
    if (mode !== "interact") return;
    const p = at(e, tapSpace);
    if (!p) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    press.current = { ...p, t: performance.now() };
  };

  const onUp = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const tp = at(e, treeSpace);
    if (mode === "inspect") {
      if (tp) setSelected(hitTest(hierarchy, tp.x, tp.y));
      return;
    }
    const start = press.current;
    press.current = null;
    if (!start || !tapSpace) return;
    const p = at(e, tapSpace) ?? start;
    batcher.flush();
    const slop = Math.max(tapSpace.width, tapSpace.height) * 0.02;
    const g = classify(start, p, performance.now() - start.t, slop);
    void send(g);
    // Clicking also selects what was clicked, for the selector panel.
    if (g.kind !== "swipe" && tp) setSelected(hitTest(hierarchy, tp.x, tp.y));
  };

  const onWheel = (e: React.WheelEvent<HTMLCanvasElement>) => {
    if (mode !== "interact" || !tapSpace) return;
    const p = at(e, tapSpace);
    if (!p) return;
    const canvas = canvasRef.current!;
    const scale = tapSpace.height / canvas.getBoundingClientRect().height;
    const w = wheel.current;
    w.dy += e.deltaY * scale;
    w.dx += e.deltaX * scale;
    w.x = p.x;
    w.y = p.y;
    if (w.timer == null) {
      w.timer = window.setTimeout(() => {
        const lim = tapSpace.height * 0.45;
        const dy = Math.round(Math.max(-lim, Math.min(lim, w.dy)));
        const dx = Math.round(Math.max(-lim, Math.min(lim, w.dx)));
        w.dy = 0;
        w.dx = 0;
        w.timer = null;
        if (Math.abs(dy) > 4 || Math.abs(dx) > 4) {
          void send({ kind: "scroll", x: w.x, y: w.y, dx, dy });
        }
      }, 200);
    }
  };

  const onKey = (e: React.KeyboardEvent<HTMLCanvasElement>) => {
    if (mode !== "interact" || e.metaKey || e.ctrlKey || e.altKey) return;
    const key = keyFor(e.key);
    if (key) {
      e.preventDefault();
      batcher.flush();
      void send({ kind: "key", key });
    } else if (e.key.length === 1) {
      e.preventDefault();
      batcher.push(e.key);
    }
  };

  const take = async () => {
    if (!deviceId) return;
    try {
      await consoleApi.takeControl(deviceId);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
    void refreshControl();
  };
  const release = async () => {
    if (!deviceId) return;
    await consoleApi.releaseControl(deviceId).catch(() => {});
    void refreshControl();
  };

  const copy = (text: string) => void navigator.clipboard?.writeText(text);

  if (!deviceId) {
    return <div className="mirror empty">Pick a device to see its screen</div>;
  }

  const busy = control?.busy;
  const box = (el: Element | null, cls: string) =>
    el && treeSpace ? (
      <rect
        className={cls}
        x={el.bounds.x}
        y={el.bounds.y}
        width={el.bounds.width}
        height={el.bounds.height}
        vectorEffect="non-scaling-stroke"
      />
    ) : null;

  return (
    <div className="mirror">
      <div className="mirror-bar">
        <div className="seg" role="group" aria-label="Mirror mode">
          <button
            className={mode === "interact" ? "on" : ""}
            onClick={() => setMode("interact")}
            title="Click taps, drag swipes, wheel scrolls, typing types"
          >
            Interact
          </button>
          <button
            className={mode === "inspect" ? "on" : ""}
            onClick={() => setMode("inspect")}
            title="Click selects an element; nothing is sent to the device"
          >
            Inspect
          </button>
        </div>
        <button onClick={() => void send({ kind: "key", key: "back" })} title="Back">
          ◀ Back
        </button>
        <button onClick={() => void send({ kind: "key", key: "home" })} title="Home">
          ● Home
        </button>
        <button onClick={() => void send({ kind: "key", key: "enter" })} title="Enter">
          ⏎
        </button>
        {isWeb && (
          <form
            className="address"
            onSubmit={(e) => {
              e.preventDefault();
              void send({ kind: "navigate", url: address.trim() });
            }}
          >
            <input
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              aria-label="Page address"
              spellCheck={false}
            />
            <button type="submit">Go</button>
          </form>
        )}
        <span className="spacer" />
        {control?.mine ? (
          <button className="secondary" onClick={release} title="Let other runs use the device">
            Release control
          </button>
        ) : busy ? (
          <span className="busy" title="Another Atalanta process holds the device">
            busy: {busy}
          </span>
        ) : (
          <button onClick={take} title="Take the device's lease">
            Take control
          </button>
        )}
      </div>

      <div className="mirror-stage">
        <div className="mirror-frame">
          <canvas
            ref={canvasRef}
            className={`mirror-canvas mode-${mode}`}
            tabIndex={0}
            width={360}
            height={780}
            onPointerMove={onMove}
            onPointerLeave={() => setHover(null)}
            onPointerDown={onDown}
            onPointerUp={onUp}
            onWheel={onWheel}
            onKeyDown={onKey}
            onBlur={() => batcher.flush()}
            aria-label="Device screen"
          />
          {treeSpace && (
            <svg
              className="mirror-overlay"
              viewBox={`0 0 ${treeSpace.width} ${treeSpace.height}`}
              preserveAspectRatio="none"
            >
              {box(selected, "sel")}
              {hover !== selected && box(hover, "hover")}
            </svg>
          )}
          {status && !status.decoded && (
            <div className="mirror-wait">
              {status.connected ? "Waiting for the first frame…" : "Connecting…"}
            </div>
          )}
        </div>
      </div>

      <div className="mirror-foot">
        <span className="hover-label">{hover ? elementLabel(hover) : " "}</span>
        {selected && generated && (
          <span className="copy-steps" title="Copy as a YAML step">
            <button className="copy-btn" onClick={() => copy(generated.yaml_tap)}>
              tap
            </button>
            <button className="copy-btn" onClick={() => copy(generated.yaml_assert)}>
              assertVisible
            </button>
            {generated.yaml_input && (
              <button className="copy-btn" onClick={() => copy(generated.yaml_input!)}>
                inputText
              </button>
            )}
          </span>
        )}
        <span className="stats" title="Painted frames per second; host-receive to paint latency">
          {status
            ? `${status.mode}${status.source ? ` · ${status.source}` : ""} · ${status.fps} fps${
                status.latencyMs != null ? ` · ${Math.round(status.latencyMs)} ms` : ""
              }`
            : ""}
        </span>
      </div>
      {status?.note && <div className="mirror-note">{status.note}</div>}
    </div>
  );
}
