// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

// Folding a task's events into what the task view shows (pure, tested).
import type { TaskEvent, TaskStatus } from "./api/console";

export interface StepRow {
  test: string;
  index: number;
  step: string;
  status: "running" | "passed" | "failed" | "skipped";
  ms: number | null;
  error: string | null;
}

/** What an explore session produced (the `result` event). */
export interface ExploreResult {
  outcome: string;
  reason: string;
  yaml: string | null;
  /** The test's file name in the task folder. */
  file: string | null;
  turns: number;
  actions: number;
  modelCalls: number;
  inputTokens: number;
  outputTokens: number;
  costUsd: number | null;
  replay: { passed: boolean; ms: number; failedStep: number | null; error: string | null }[];
  repair: string | null;
}

export interface TaskSummary {
  status: TaskStatus;
  summary: string;
  steps: StepRow[];
  explore: TaskEvent[];
  log: string[];
  acceptsInstructions: boolean;
  result: ExploreResult | null;
  /** Where the test was saved in the project, if it was. */
  saved: string | null;
}

const MAX_LOG = 5000;

export function summarize(events: TaskEvent[]): TaskSummary {
  const out: TaskSummary = {
    status: "queued",
    summary: "",
    steps: [],
    explore: [],
    log: [],
    acceptsInstructions: false,
    result: null,
    saved: null,
  };
  const at = new Map<string, number>();
  for (const e of events) {
    const d = e.data;
    switch (e.kind) {
      case "status":
        out.status = d.status as TaskStatus;
        if (typeof d.summary === "string" && d.summary) out.summary = d.summary;
        break;
      case "stepStarted": {
        const key = `${String(d.test)}/${Number(d.index)}`;
        at.set(key, out.steps.length);
        out.steps.push({
          test: String(d.test ?? ""),
          index: Number(d.index ?? 0),
          step: String(d.step ?? ""),
          status: "running",
          ms: null,
          error: null,
        });
        break;
      }
      case "stepFinished": {
        const key = `${String(d.test)}/${Number(d.index)}`;
        const row: StepRow = {
          test: String(d.test ?? ""),
          index: Number(d.index ?? 0),
          step: String(d.step ?? ""),
          status: (d.status as StepRow["status"]) ?? "passed",
          ms: typeof d.ms === "number" ? d.ms : null,
          error: typeof d.error === "string" ? d.error : null,
        };
        const i = at.get(key);
        if (i != null) out.steps[i] = row;
        else {
          at.set(key, out.steps.length);
          out.steps.push(row);
        }
        break;
      }
      case "testStarted":
        // A retry starts the test's steps over.
        for (const k of [...at.keys()]) if (k.startsWith(`${String(d.test)}/`)) at.delete(k);
        break;
      case "explore":
      case "instruction":
        out.explore.push(e);
        if (e.kind === "instruction") out.acceptsInstructions = true;
        break;
      case "result":
        out.result = d as unknown as ExploreResult;
        break;
      case "saved":
        out.saved = String(d.path ?? "");
        break;
      case "log":
        if (out.log.length < MAX_LOG) out.log.push(String(d.line ?? ""));
        break;
    }
  }
  if (out.explore.length && out.status === "running") out.acceptsInstructions = true;
  return out;
}
