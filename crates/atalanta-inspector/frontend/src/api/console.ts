// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

// The console's API: control, input, tasks, suites, diagnostics.

export interface ControlStatus {
  deviceId: string;
  mine: boolean;
  heldSecs: number | null;
  /** Who else holds the device's lease, if anyone. */
  busy: string | null;
  task: number | null;
  /** The size taps are given in (device px on Android, points on iOS). */
  screen: { width: number; height: number } | null;
}

export type InputMsg =
  | { kind: "tap"; x: number; y: number }
  | { kind: "longPress"; x: number; y: number; ms: number }
  | { kind: "swipe"; from: [number, number]; to: [number, number] }
  | { kind: "scroll"; x: number; y: number; dx: number; dy: number }
  | { kind: "key"; key: string }
  | { kind: "text"; text: string }
  | { kind: "navigate"; url: string };

export type TaskStatus = "queued" | "running" | "passed" | "failed" | "cancelled" | "error";

export interface TaskInfo {
  id: number;
  kind: "run" | "explore";
  title: string;
  deviceId: string;
  status: TaskStatus;
  createdMs: number;
  startedMs: number | null;
  finishedMs: number | null;
  summary: string;
  acceptsInstructions: boolean;
  events: number;
}

export interface TaskEvent {
  seq: number;
  t: number;
  kind: string;
  data: Record<string, unknown>;
}

export interface Suite {
  path: string;
  tests: string[];
}

export interface ArtifactFile {
  path: string;
  size: number;
}

const enc = encodeURIComponent;

async function call<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status}: ${text}`);
  }
  return res.json();
}

const post = (body?: unknown): RequestInit => ({
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: body === undefined ? undefined : JSON.stringify(body),
});

export const controlStatus = (id: string) =>
  call<ControlStatus>(`/api/devices/${enc(id)}/control`);

export const takeControl = (id: string) =>
  call<{ ok: boolean }>(`/api/devices/${enc(id)}/control`, post());

export const releaseControl = (id: string) =>
  call<{ ok: boolean }>(`/api/devices/${enc(id)}/control`, { method: "DELETE" });

export const sendInput = (id: string, msg: InputMsg) =>
  call<{ ok: boolean }>(`/api/devices/${enc(id)}/input`, post(msg));

export const listTasks = () =>
  call<{
    tasks: TaskInfo[];
    exploreAvailable: boolean;
    exploreMessage: string | null;
    /** The console's `--app-id`, if it was given. */
    appId: string | null;
  }>("/api/tasks");

export const getTask = (id: number) =>
  call<TaskInfo & { eventList: TaskEvent[] }>(`/api/tasks/${id}`);

export const startRun = (suite: string, deviceId: string, filter?: string) =>
  call<TaskInfo>("/api/tasks/run", post({ suite, deviceId, filter }));

export interface ExploreOptions {
  goal: string;
  deviceId: string;
  appId?: string;
  /** A plan file in the project (`--script`) instead of a model. */
  script?: string;
  /** `provider:model`. */
  model?: string;
  replay?: number;
}

export const startExplore = (o: ExploreOptions) => call<TaskInfo>("/api/tasks/explore", post(o));

/** Write an explore task's test into the project folder. */
export const saveExplore = (id: number, path: string, overwrite = false) =>
  call<{ ok: boolean; path: string }>(`/api/tasks/${id}/save`, post({ path, overwrite }));

export const injectInstruction = (id: number, text: string) =>
  call<{ ok: boolean }>(`/api/tasks/${id}/inject`, post({ text }));

export const cancelTask = (id: number) => call<{ ok: boolean }>(`/api/tasks/${id}/cancel`, post());

export const taskFiles = (id: number) =>
  call<{ files: ArtifactFile[] }>(`/api/tasks/${id}/files`);

export function taskFileUrl(id: number, path: string): string {
  return `/api/tasks/${id}/files/${path.split("/").map(enc).join("/")}`;
}

export const listSuites = () =>
  call<{ projectDir: string; suites: Suite[] }>("/api/project/suites");

export interface Diagnostics {
  available: boolean;
  report?: unknown;
  message?: string;
}

export async function diagnostics(): Promise<Diagnostics> {
  const res = await fetch("/api/diagnostics");
  try {
    return await res.json();
  } catch {
    return { available: false, message: `${res.status}` };
  }
}

/** Live events of a task (all past ones first). Returns a closer. */
export function taskEvents(
  id: number,
  onEvent: (e: TaskEvent) => void,
  onEnd: () => void,
): () => void {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(`${proto}//${location.host}/api/tasks/${id}/events`);
  ws.onmessage = (m) => {
    try {
      onEvent(JSON.parse(m.data));
    } catch {
      // ignore
    }
  };
  ws.onclose = () => onEnd();
  return () => {
    ws.onclose = null;
    ws.close();
  };
}
