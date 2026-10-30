import { useCallback, useEffect, useState } from "react";
import * as consoleApi from "./api/console";
import type { TaskInfo } from "./api/console";
import { ActionToolbar } from "./components/ActionToolbar";
import { ConnectPanel, ExplorePanel, RunPanel, TasksPanel } from "./components/ConsolePanels";
import { DevicePicker } from "./components/DevicePicker";
import { ElementTree } from "./components/ElementTree";
import { Mirror } from "./components/Mirror";
import { PerformanceChart } from "./components/PerformanceChart";
import { PropertiesPanel } from "./components/PropertiesPanel";
import { RecordingPanel } from "./components/RecordingPanel";
import { SelectorDisplay } from "./components/SelectorDisplay";
import { useWebSocket } from "./hooks/useWebSocket";
import { useInspectorStore } from "./store/inspectorStore";

type Tab = "inspect" | "run" | "explore" | "tasks" | "connect";

const TABS: { key: Tab; title: string }[] = [
  { key: "inspect", title: "Inspect" },
  { key: "run", title: "Run" },
  { key: "explore", title: "Explore" },
  { key: "tasks", title: "Tasks" },
  { key: "connect", title: "Devices" },
];

export default function App() {
  useWebSocket();
  const { error, setError, isRecording, recordedSteps } = useInspectorStore();
  const showRecording = isRecording || recordedSteps.length > 0;
  const [tab, setTab] = useState<Tab>("inspect");
  const [tasks, setTasks] = useState<TaskInfo[]>([]);
  const [explore, setExplore] = useState<{ available: boolean; message: string | null; appId: string | null }>({
    available: false,
    message: null,
    appId: null,
  });
  const [selectedTask, setSelectedTask] = useState<number | null>(null);

  const refreshTasks = useCallback(async () => {
    try {
      const r = await consoleApi.listTasks();
      setTasks(r.tasks);
      setExplore({ available: r.exploreAvailable, message: r.exploreMessage, appId: r.appId });
    } catch {
      // shown elsewhere
    }
  }, []);

  useEffect(() => {
    const first = setTimeout(() => void refreshTasks(), 0);
    const t = setInterval(() => {
      if (!document.hidden) void refreshTasks();
    }, 2000);
    return () => {
      clearTimeout(first);
      clearInterval(t);
    };
  }, [refreshTasks]);

  const onTask = (t: TaskInfo) => {
    setSelectedTask(t.id);
    void refreshTasks();
  };
  const running = tasks.filter((t) => t.status === "running" || t.status === "queued").length;

  return (
    <div className="app">
      <header className="app-header">
        <div className="header-left">
          <h1>Velocity Console</h1>
          <DevicePicker />
        </div>
        <ActionToolbar />
      </header>

      {error && (
        <div className="error-bar" onClick={() => setError(null)}>
          {error}
        </div>
      )}

      <main className="app-main">
        <section className="panel-left">
          <Mirror />
          <PerformanceChart />
        </section>
        <section className="panel-right">
          <nav className="tabs">
            {TABS.map((t) => (
              <button
                key={t.key}
                className={tab === t.key ? "on" : ""}
                onClick={() => setTab(t.key)}
              >
                {t.title}
                {t.key === "tasks" && running > 0 && <span className="badge">{running}</span>}
              </button>
            ))}
          </nav>
          <div className={`tab-body ${tab === "inspect" && showRecording ? "with-recording" : ""}`}>
            {tab === "inspect" && (
              <>
                <ElementTree />
                <PropertiesPanel />
                <SelectorDisplay />
                <RecordingPanel />
              </>
            )}
            {tab === "run" && <RunPanel onTask={onTask} />}
            {tab === "explore" && (
              <ExplorePanel
                available={explore.available}
                message={explore.message}
                appId={explore.appId}
                onTask={onTask}
              />
            )}
            {tab === "tasks" && (
              <TasksPanel tasks={tasks} selected={selectedTask} onSelect={setSelectedTask} />
            )}
            {tab === "connect" && <ConnectPanel />}
          </div>
        </section>
      </main>
    </div>
  );
}
