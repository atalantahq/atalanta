import { useCallback, useEffect, useState } from "react";
import * as api from "../api/console";
import type { Suite, TaskInfo } from "../api/console";
import { listDevices, selectDevice } from "../api/client";
import type { DeviceInfo } from "../api/types";
import { useInspectorStore } from "../store/inspectorStore";
import { TaskView } from "./TaskView";

const PLATFORMS: { key: DeviceInfo["platform"] | "web"; title: string }[] = [
  { key: "android", title: "Android" },
  { key: "ios", title: "iOS" },
  { key: "web", title: "Web" },
];

function useSelect() {
  const setCurrentDeviceId = useInspectorStore((s) => s.setCurrentDeviceId);
  const setError = useInspectorStore((s) => s.setError);
  const [selecting, setSelecting] = useState<string | null>(null);
  const select = async (id: string) => {
    setSelecting(id);
    try {
      await selectDevice(id);
      setCurrentDeviceId(id);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Failed to select device");
    } finally {
      setSelecting(null);
    }
  };
  return { select, selecting };
}

interface DoctorCheck {
  id?: string;
  area?: string;
  status?: string;
  summary?: string;
  detail?: string;
  fix?: { kind?: string; value?: string };
}

/** Diagnostics: `velocity doctor`'s report (verdict, then each check). */
function DiagnosticsView() {
  const [d, setD] = useState<api.Diagnostics | null>(null);
  const [loading, setLoading] = useState(false);
  const run = async () => {
    setLoading(true);
    try {
      setD(await api.diagnostics());
    } finally {
      setLoading(false);
    }
  };
  const report = d?.report as { verdict?: string; checks?: unknown } | undefined;
  const checks = Array.isArray(report?.checks) ? (report.checks as DoctorCheck[]) : null;
  return (
    <div className="diag">
      <div className="row">
        <h4>Diagnostics</h4>
        <button onClick={() => void run()} disabled={loading}>
          {loading ? "Checking…" : d ? "Check again" : "Run checks"}
        </button>
      </div>
      {d && !d.available && <div className="dim">{d.message}</div>}
      {d?.available && d.message && <div className="inline-error">{d.message}</div>}
      {report?.verdict && (
        <div className={`verdict verdict-${report.verdict}`}>
          {report.verdict === "ready" ? "Ready" : report.verdict === "degraded" ? "Ready, with warnings" : "Blocked"}
        </div>
      )}
      {checks && (
        <ul className="checks">
          {checks
            .filter((c) => c.status !== "skip")
            .map((c, i) => (
              <li key={c.id ?? i} className={`check check-${c.status ?? ""}`}>
                <span className="check-status">{c.status}</span> <b>{c.id}</b> {c.summary}
                {c.detail && <div className="dim">{c.detail}</div>}
                {c.fix?.value && (
                  <div className="dim">
                    {c.fix.kind === "run" ? "run: " : "fix: "}
                    <code>{c.fix.value}</code>
                  </div>
                )}
              </li>
            ))}
        </ul>
      )}
      {d?.report != null && !checks && <pre>{JSON.stringify(d.report, null, 2)}</pre>}
    </div>
  );
}

/** Connection wizard: every platform's devices, their state, and checks. */
export function ConnectPanel() {
  const devices = useInspectorStore((s) => s.devices);
  const setDevices = useInspectorStore((s) => s.setDevices);
  const current = useInspectorStore((s) => s.currentDeviceId);
  const { select, selecting } = useSelect();
  const [busy, setBusy] = useState<Record<string, string | null>>({});
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const list = await listDevices();
      setDevices(list);
      const entries = await Promise.all(
        list
          .filter((d) => d.state === "booted" && d.platform !== "web")
          .map(async (d) => {
            try {
              const c = await api.controlStatus(d.id);
              return [d.id, c.busy] as const;
            } catch {
              return [d.id, null] as const;
            }
          }),
      );
      setBusy(Object.fromEntries(entries));
    } catch {
      // the error bar shows list failures elsewhere
    } finally {
      setRefreshing(false);
    }
  }, [setDevices]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <div className="panel-body connect">
      <div className="row">
        <h3>Devices</h3>
        <button onClick={() => void refresh()} disabled={refreshing}>
          {refreshing ? "Refreshing…" : "Refresh"}
        </button>
      </div>
      {PLATFORMS.map(({ key, title }) => {
        const list = devices.filter((d) => d.platform === key);
        return (
          <div key={key} className="platform-group">
            <h4>{title}</h4>
            {list.length === 0 && <div className="dim">None found.</div>}
            {list.map((d) => (
              <div key={d.id} className={`device-row ${d.id === current ? "current" : ""}`}>
                <span className={`dot state-${d.state}`} title={d.state} />
                <span className="device-name">
                  {d.name}
                  <span className="dim">
                    {" "}
                    {d.os_version ?? ""} {d.device_type !== "unknown" ? d.device_type : ""} · {d.id}
                  </span>
                </span>
                {busy[d.id] && <span className="busy">busy: {busy[d.id]}</span>}
                {d.id === current ? (
                  <span className="dim">connected</span>
                ) : (
                  <button
                    disabled={d.state !== "booted" || selecting !== null}
                    onClick={() => void select(d.id)}
                    title={d.state !== "booted" ? "Boot it first (velocity devices up)" : "Mirror and inspect it"}
                  >
                    {selecting === d.id ? "Connecting…" : "Connect"}
                  </button>
                )}
              </div>
            ))}
          </div>
        );
      })}
      <DiagnosticsView />
    </div>
  );
}

/** Run a suite (or one test) on the current device, with live steps. */
export function RunPanel({ onTask }: { onTask: (t: TaskInfo) => void }) {
  const deviceId = useInspectorStore((s) => s.currentDeviceId);
  const [suites, setSuites] = useState<Suite[]>([]);
  const [projectDir, setProjectDir] = useState("");
  const [suite, setSuite] = useState("");
  const [test, setTest] = useState("");
  const [task, setTask] = useState<TaskInfo | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    api
      .listSuites()
      .then((r) => {
        setSuites(r.suites);
        setProjectDir(r.projectDir);
        if (r.suites.length) setSuite((s) => s || r.suites[0].path);
      })
      .catch((e) => setErr(String(e.message ?? e)));
  }, []);

  const tests = suites.find((s) => s.path === suite)?.tests ?? [];

  const run = async () => {
    if (!deviceId || !suite) return;
    setErr(null);
    try {
      const t = await api.startRun(suite, deviceId, test || undefined);
      setTask(t);
      onTask(t);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="panel-body run">
      <div className="dim small">Suites in {projectDir || "the project folder"}</div>
      <div className="form">
        <select value={suite} onChange={(e) => { setSuite(e.target.value); setTest(""); }}>
          {suites.length === 0 && <option value="">No suites found</option>}
          {suites.map((s) => (
            <option key={s.path} value={s.path}>
              {s.path}
            </option>
          ))}
        </select>
        <select value={test} onChange={(e) => setTest(e.target.value)}>
          <option value="">All tests</option>
          {tests.map((t) => (
            <option key={t} value={t}>
              {t}
            </option>
          ))}
        </select>
        <button onClick={() => void run()} disabled={!deviceId || !suite}>
          Run on {deviceId ?? "…"}
        </button>
      </div>
      {err && <div className="inline-error">{err}</div>}
      {task && <TaskView key={task.id} id={task.id} />}
    </div>
  );
}

/** Remembered per browser: the explore form's app and plan. */
function remembered(key: string): string {
  try {
    return localStorage.getItem(`velocity.explore.${key}`) ?? "";
  } catch {
    return "";
  }
}

function remember(key: string, value: string) {
  try {
    localStorage.setItem(`velocity.explore.${key}`, value);
  } catch {
    // private mode: not remembered
  }
}

/** A goal for the explore agent, its live turns, steering, and the test
 *  it writes (replayed with no model, then saved into the project). */
export function ExplorePanel({
  available,
  message,
  appId: defaultApp,
  onTask,
}: {
  available: boolean;
  message: string | null;
  appId: string | null;
  onTask: (t: TaskInfo) => void;
}) {
  const deviceId = useInspectorStore((s) => s.currentDeviceId);
  const [goal, setGoal] = useState("");
  const [appId, setAppId] = useState(() => remembered("appId"));
  const [script, setScript] = useState(() => remembered("script"));
  const [model, setModel] = useState("");
  const [task, setTask] = useState<TaskInfo | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const app = appId.trim() || defaultApp || "";

  const start = async () => {
    if (!deviceId || !goal.trim() || !app) return;
    setErr(null);
    remember("appId", appId.trim());
    remember("script", script.trim());
    try {
      const t = await api.startExplore({
        goal: goal.trim(),
        deviceId,
        appId: app,
        script: script.trim() || undefined,
        model: model.trim() || undefined,
      });
      setTask(t);
      onTask(t);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="panel-body explore">
      {!available && (
        <div className="unavailable">{message ?? "Explore is not available in this build."}</div>
      )}
      <textarea
        value={goal}
        onChange={(e) => setGoal(e.target.value)}
        placeholder="What should it do? e.g. “open Network & internet”"
        rows={3}
        disabled={!available}
      />
      <div className="form">
        <input
          value={appId}
          onChange={(e) => setAppId(e.target.value)}
          placeholder={defaultApp ?? "App id (com.example.app)"}
          aria-label="App id"
          disabled={!available}
        />
        <input
          value={script}
          onChange={(e) => setScript(e.target.value)}
          placeholder="Plan file in the project (optional, no model)"
          aria-label="Plan file"
          disabled={!available}
        />
        <input
          value={model}
          onChange={(e) => setModel(e.target.value)}
          placeholder="Model (provider:model, optional)"
          aria-label="Model"
          disabled={!available || !!script.trim()}
        />
      </div>
      <div className="form">
        <button onClick={() => void start()} disabled={!available || !deviceId || !goal.trim() || !app}>
          Explore on {deviceId ?? "…"}
        </button>
      </div>
      {err && <div className="inline-error">{err}</div>}
      {task && <TaskView key={task.id} id={task.id} acceptsInstructions />}
    </div>
  );
}

function ago(ms: number) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  return `${Math.round(s / 3600)}h ago`;
}

/** Every task started from this console: queue, history and replay. */
export function TasksPanel({
  tasks,
  selected,
  onSelect,
}: {
  tasks: TaskInfo[];
  selected: number | null;
  onSelect: (id: number) => void;
}) {
  const sel = tasks.find((t) => t.id === selected) ?? null;
  return (
    <div className="panel-body tasks">
      {tasks.length === 0 && <div className="dim">No tasks yet: start a run or an explore.</div>}
      <ul className="task-list">
        {tasks.map((t) => (
          <li
            key={t.id}
            className={t.id === selected ? "selected" : ""}
            onClick={() => onSelect(t.id)}
          >
            <span className={`status status-${t.status}`}>{t.status}</span>
            <span className="task-kind">{t.kind}</span>
            <span className="task-title">#{t.id} {t.title}</span>
            <span className="dim">
              {t.deviceId} · {ago(t.createdMs)}
            </span>
          </li>
        ))}
      </ul>
      {sel && <TaskView key={sel.id} id={sel.id} acceptsInstructions={sel.acceptsInstructions} />}
    </div>
  );
}
