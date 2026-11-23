// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import type { WsMessage } from "./types";

export type WsListener = (msg: WsMessage) => void;

export class InspectorSocket {
  private ws: WebSocket | null = null;
  private listeners: WsListener[] = [];
  private deviceId: string;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  // Set by disconnect(): a closed socket must not reconnect itself (VEL-115).
  private closed = false;
  // Reconnect back-off: 1, 2, 4 … 30 s, reset once a socket opens.
  private retryMs = 1000;

  constructor(deviceId: string) {
    this.deviceId = deviceId;
  }

  connect() {
    if (this.closed) return;
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    // The mirror shows the screen: only the hierarchy (and errors) here.
    const url = `${proto}//${location.host}/api/ws/${encodeURIComponent(this.deviceId)}?screenshots=0`;

    this.ws = new WebSocket(url);

    this.ws.onopen = () => {
      this.retryMs = 1000;
    };

    this.ws.onmessage = (event) => {
      try {
        const msg: WsMessage = JSON.parse(event.data);
        this.listeners.forEach((fn) => fn(msg));
      } catch {
        // Ignore malformed messages
      }
    };

    this.ws.onclose = () => {
      if (this.closed) return;
      this.reconnectTimer = setTimeout(() => this.connect(), this.retryMs);
      this.retryMs = Math.min(this.retryMs * 2, 30_000);
    };

    this.ws.onerror = () => {
      this.ws?.close();
    };
  }

  onMessage(fn: WsListener) {
    this.listeners.push(fn);
    return () => {
      this.listeners = this.listeners.filter((l) => l !== fn);
    };
  }

  sendRefresh() {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ type: "refresh" }));
    }
  }

  disconnect() {
    this.closed = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    if (this.ws) {
      this.ws.onopen = null;
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.close();
    }
    this.ws = null;
    this.listeners = [];
  }
}
