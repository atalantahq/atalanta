import type { DeviceInfo, Direction, Element, GenerateResponse, Selector } from "./types";

const BASE = "/api";

async function json<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status}: ${text}`);
  }
  return res.json();
}

export async function listDevices(): Promise<DeviceInfo[]> {
  return json(`${BASE}/devices`);
}

export async function currentDevice(): Promise<string | null> {
  const r = await json<{ deviceId: string | null }>(`${BASE}/devices/current`);
  return r.deviceId;
}

export async function selectDevice(deviceId: string): Promise<DeviceInfo> {
  return json(`${BASE}/devices/select`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ device_id: deviceId }),
  });
}

export function screenshotUrl(deviceId: string): string {
  return `${BASE}/devices/${encodeURIComponent(deviceId)}/screenshot?t=${Date.now()}`;
}

export async function getHierarchy(deviceId: string): Promise<Element> {
  return json(`${BASE}/devices/${encodeURIComponent(deviceId)}/hierarchy`);
}

export async function tap(deviceId: string, opts: { selector?: Selector; coordinates?: [number, number] }): Promise<void> {
  await json(`${BASE}/devices/${encodeURIComponent(deviceId)}/tap`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(opts),
  });
}

// Returns the recorded step (`yaml`), built by the server so it always parses.
export async function typeText(
  deviceId: string,
  selector: Selector,
  text: string,
): Promise<{ yaml: string }> {
  return json(`${BASE}/devices/${encodeURIComponent(deviceId)}/type`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ selector, text }),
  });
}

export async function swipe(deviceId: string, direction: Direction): Promise<{ yaml: string }> {
  return json(`${BASE}/devices/${encodeURIComponent(deviceId)}/swipe`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ direction }),
  });
}

// The server resolves the node in the hierarchy snapshot it sent (by key
// and version), checks the selector is unique there, and lists every
// candidate with its match count. The tree is never posted back (VEL-116).
export async function generateSelector(
  nodeKey: string,
  version: number,
): Promise<GenerateResponse> {
  return json(`${BASE}/selector/generate`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ nodeKey, version }),
  });
}

export async function saveFlow(
  name: string,
  appId: string,
  steps: string[],
  path?: string
): Promise<{ path: string; yaml: string }> {
  return json(`${BASE}/flow/save`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, app_id: appId, steps, path }),
  });
}
