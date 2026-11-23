import { useEffect, useMemo, useState } from "react";
import * as api from "../api/console";
import type { ArtifactFile, TaskEvent } from "../api/console";
import { summarize, type ExploreResult, type StepRow } from "../tasks";

function statusClass(s: string) {
  return `status status-${s}`;
}

function fmtSize(n: number) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1 << 20)).toFixed(1)} MB`;
}

/** An explore event: its name (`{"event":"planned",…}`) and its fields. */
function exploreVariant(data: Record<string, unknown>): [string, Record<string, unknown>] {
  if (typeof data.event === "string") return [data.event, data];
  if (typeof data.type === "string") return [data.type, data];
  const [k, v] = Object.entries(data)[0] ?? ["event", {}];
  return [k, (v && typeof v === "object" ? v : { value: v }) as Record<string, unknown>];
}

/** Fields not worth a column. */
const HIDDEN = new Set(["event", "type", "atMs", "t", "hash"]);

function ExploreLine({ ev }: { ev: TaskEvent }) {
  const [variant, body] = exploreVariant(ev.data);
  const rest = Object.entries(body).filter(([k, v]) => !HIDDEN.has(k) && v != null && v !== "");
  return (
    <div className={`explore-ev ev-${variant.toLowerCase()}`}>
      <span className="ev-kind">{variant}</span>
      {rest.map(([k, v]) => (
        <span key={k} className="ev-field">
          <span className="ev-key">{k}</span>
          {typeof v === "string" && v.includes("\n") ? (
            <pre>{v}</pre>
          ) : (
            <span className="ev-val">{typeof v === "string" ? v : JSON.stringify(v)}</span>
          )}
        </span>
      ))}
    </div>
  );
}

/** The test an explore session wrote, its replays, and saving it. */
function ExploreResultView({ id, result, saved }: { id: number; result: ExploreResult; saved: string | null }) {
  const [path, setPath] = useState(`flows/${result.file ?? "explore.yaml"}`);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const save = async (overwrite: boolean) => {
    setErr(null);
    try {
      const r = await api.saveExplore(id, path.trim(), overwrite);
      setMsg(`Saved to ${r.path}: run it with atalanta run ${r.path}`);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };
  const passed = result.replay.filter((r) => r.passed).length;
  return (
    <div className="explore-result">
      <div>
        <span className={`status status-${result.outcome === "success" ? "passed" : "failed"}`}>{result.outcome}</span>{" "}
        {result.reason}
      </div>
      <div className="dim small">
        {result.turns} turns, {result.actions} actions, {result.modelCalls ?? 0} model calls,{" "}
        {result.inputTokens ?? 0}/{result.outputTokens ?? 0} tokens
        {result.costUsd != null ? `, est. $${result.costUsd.toFixed(4)}` : ""}
      </div>
      {result.replay.length > 0 && (
        <ul className="replays">
          {result.replay.map((r, i) => (
            <li key={i}>
              <span className={`status status-${r.passed ? "passed" : "failed"}`}>replay {i + 1}</span>{" "}
              {(r.ms / 1000).toFixed(1)} s
              {!r.passed && r.failedStep != null ? ` · step ${r.failedStep + 1}: ${r.error ?? ""}` : ""}
            </li>
          ))}
          <li className="dim">
            {passed}/{result.replay.length} passed with no model calls
          </li>
        </ul>
      )}
      {result.repair && <div className="dim">repair: {result.repair}</div>}
      {result.yaml && (
        <>
          <pre className="yaml">{result.yaml}</pre>
          {saved || msg ? (
            <div className="ok">{msg ?? `Saved to ${saved}`}</div>
          ) : (
            <div className="form">
              <input value={path} onChange={(e) => setPath(e.target.value)} aria-label="Save as" />
              <button onClick={() => void save(false)} disabled={!path.trim()}>
                Save to project
              </button>
              {err?.startsWith("409") && (
                <button className="secondary" onClick={() => void save(true)}>
                  Overwrite
                </button>
              )}
            </div>
          )}
          {err && <div className="inline-error">{err}</div>}
        </>
      )}
    </div>
  );
}

/** One task: live status, steps, explore stream or log, and artifacts. */
export function TaskView({ id, acceptsInstructions = false }: { id: number; acceptsInstructions?: boolean }) {
  const [events, setEvents] = useState<TaskEvent[]>([]);
  const [files, setFiles] = useState<ArtifactFile[]>([]);
  const [instruction, setInstruction] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [showLog, setShowLog] = useState(false);

  // Mounted per task (`key={id}`), so the state starts empty for each.
  useEffect(() => {
    const close = api.taskEvents(
      id,
      (e) => setEvents((prev) => (prev.length > 20000 ? [...prev.slice(-15000), e] : [...prev, e])),
      () => {
        api.taskFiles(id).then((f) => setFiles(f.files)).catch(() => {});
      },
    );
    return close;
  }, [id]);

  const s = useMemo(() => summarize(events), [events]);
  const done = s.status !== "queued" && s.status !== "running";

  useEffect(() => {
    if (done) api.taskFiles(id).then((f) => setFiles(f.files)).catch(() => {});
  }, [done, id]);

  const traces = files.filter((f) => f.path.endsWith("trace.html"));
  const media = files.filter((f) => /\.(mp4|log|txt|html|json|xml|png)$/.test(f.path) && !f.path.endsWith("trace.html"));

  const inject = async () => {
    const text = instruction.trim();
    if (!text) return;
    try {
      await api.injectInstruction(id, text);
      setInstruction("");
      setErr(null);
    } catch (e: unknown) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="task-view">
      <div className="task-head">
        <span className={statusClass(s.status)}>{s.status}</span>
        <span className="task-sum">{s.summary}</span>
        {!done && (
          <button className="secondary" onClick={() => void api.cancelTask(id)}>
            Cancel
          </button>
        )}
      </div>

      {s.steps.length > 0 && (
        <table className="steps">
          <thead>
            <tr>
              <th>Test</th>
              <th>#</th>
              <th>Step</th>
              <th>Status</th>
              <th className="num">ms</th>
            </tr>
          </thead>
          <tbody>
            {s.steps.map((r: StepRow) => (
              <tr key={`${r.test}/${r.index}`} className={`row-${r.status}`}>
                <td>{r.test}</td>
                <td>{r.index + 1}</td>
                <td className="mono">
                  {r.step}
                  {r.error && <div className="step-error">{r.error}</div>}
                </td>
                <td>
                  <span className={statusClass(r.status)}>{r.status}</span>
                </td>
                <td className="num">{r.ms ?? ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {s.explore.length > 0 && (
        <div className="explore-stream">
          {s.explore.map((e) =>
            e.kind === "instruction" ? (
              <div key={e.seq} className="explore-ev ev-instruction">
                <span className="ev-kind">You</span>
                <span className="ev-val">{String(e.data.text ?? "")}</span>
              </div>
            ) : (
              <ExploreLine key={e.seq} ev={e} />
            ),
          )}
        </div>
      )}

      {s.result && <ExploreResultView id={id} result={s.result} saved={s.saved} />}

      {(acceptsInstructions || s.acceptsInstructions) && !done && (
        <div className="inject">
          <input
            value={instruction}
            onChange={(e) => setInstruction(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && void inject()}
            placeholder="Steer it: e.g. “use the search box instead”"
          />
          <button onClick={() => void inject()} disabled={!instruction.trim()}>
            Inject
          </button>
        </div>
      )}
      {err && <div className="inline-error">{err}</div>}

      {(traces.length > 0 || media.length > 0) && (
        <div className="artifacts">
          <h4>Artifacts</h4>
          {traces.map((f) => (
            <a key={f.path} href={api.taskFileUrl(id, f.path)} target="_blank" rel="noreferrer" className="replay">
              ▶ Replay {f.path.split("/").slice(-2, -1)[0] ?? "trace"} (video synced to steps)
            </a>
          ))}
          <ul>
            {media.map((f) => (
              <li key={f.path}>
                <a href={api.taskFileUrl(id, f.path)} target="_blank" rel="noreferrer">
                  {f.path}
                </a>{" "}
                <span className="dim">{fmtSize(f.size)}</span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {s.log.length > 0 && (
        <div className="task-log">
          <button className="link" onClick={() => setShowLog(!showLog)}>
            {showLog ? "Hide" : "Show"} output ({s.log.length} lines)
          </button>
          {showLog && <pre>{s.log.join("\n")}</pre>}
        </div>
      )}
    </div>
  );
}

