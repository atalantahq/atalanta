#!/usr/bin/env python3
"""MiniWoB++ through Velocity's web agent.

MiniWoB++ (Farama-Foundation/miniwob-plusplus, the commit BrowserGym pins)
is a set of small web tasks, each with a programmatic reward computed by the
page itself (`core.endEpisode`). This harness:

- serves the task pages and injects a script into each one that seeds the
  task (`Math.seedrandom`), starts the episode (`core.startEpisodeReal`),
  lifts the 10 s episode limit (as BrowserGym does for model agents) and
  reports MiniWoB's own reward to the server when the episode ends;
- runs `velocity explore` once per task on headless Chrome: launch = load the
  page, goal = "do what the instruction at the top of the page says" (the
  task's utterance is on screen in `#query`), `--values-from-screen` so text
  the instruction quotes may be typed, no test written (`--dry-run`);
- scores success as MiniWoB's raw reward > 0, and reports the mean raw reward.

    python3 run.py --out out/run1 [--tasks click-button,enter-text] [--limit N]
    python3 run.py --summary out/run1

Model and provider come from the environment (docs/AI-PROVIDERS.md).
"""

import argparse
import json
import os
import subprocess
import threading
import time
import urllib.parse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
HTML = ROOT / "ext" / "miniwob-plusplus" / "miniwob" / "html"
V = os.environ.get("VELOCITY_BIN", str(ROOT / "target" / "debug" / "velocity"))
PORT = int(os.environ.get("MINIWOB_PORT", "8791"))
REWARDS = {}
LOCK = threading.Lock()
GOAL = ("Do exactly what the instruction at the top of the page says (the text above the task "
        "area), then stop.")

# The reward display is hidden: after the agent's action, endEpisode
# still writes "reward: -1 / 1" there, which the agent would read (a -1
# invites fruitless extra turns, a +1 is a done-signal real pages don't
# have; review 10 #12). The reward is reported through /__reward only.
INJECT = """<style>#reward-display { display: none !important; }</style>
<script>
(function () {
  var task = %(task)s, seed = %(seed)s;
  core.EPISODE_MAX_TIME = 1e9;
  var orig = core.endEpisode;
  core.endEpisode = function (reward, timeProportional, reason) {
    var live = core.EP_TIMER !== null;
    orig.apply(this, arguments);
    if (live) {
      fetch('/__reward?task=' + task + '&raw=' + WOB_RAW_REWARD_GLOBAL +
            '&reason=' + encodeURIComponent(reason || ''));
    }
  };
  // Start the episode whenever the page asks for one: some tasks call
  // core.startEpisode after their own setup, which showed the START cover
  // again over a running episode (click-dialog, moving-items).
  // One episode per run: MiniWoB calls core.startEpisode again after an
  // episode ends (for the next one); that would replace the finished task
  // with a new one under the agent.
  var started = false;
  var begin = function () {
    if (started) { return; }
    started = true;
    Math.seedrandom(seed);
    core.startEpisodeReal();
  };
  core.startEpisode = function () {
    core.createDisplay();
    if (core.cover_div == null) {
      core.cover_div = document.createElement('div');
      core.cover_div.setAttribute('id', 'sync-task-cover');
      core.cover_div.style.display = 'none';
      document.body.appendChild(core.cover_div);
    }
    begin();
  };
  window.addEventListener('load', function () {
    setTimeout(begin, 0);
  });
})();
</script>"""


class Handler(SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=str(HTML), **kw)

    def log_message(self, *a):
        pass

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        if u.path == "/__reward":
            q = urllib.parse.parse_qs(u.query)
            with LOCK:
                # The run's one episode: never overwritten.
                REWARDS.setdefault(q["task"][0], {"raw": float(q["raw"][0]),
                                                  "reason": q.get("reason", [""])[0]})
            self.send_response(204)
            self.end_headers()
            return
        if u.path.startswith("/miniwob/") and u.path.endswith(".html"):
            task = Path(u.path).stem
            body = (HTML / u.path.lstrip("/")).read_text()
            script = INJECT % {"task": json.dumps(task), "seed": json.dumps(f"velocity-{task}")}
            body = body.replace("</body>", script + "</body>") if "</body>" in body else body + script
            data = body.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        super().do_GET()


def tasks_all():
    return sorted(p.stem for p in (HTML / "miniwob").glob("*.html"))


def run_task(task, out):
    d = out / task
    d.mkdir(parents=True, exist_ok=True)
    with LOCK:
        REWARDS.pop(task, None)
    url = f"http://127.0.0.1:{PORT}/miniwob/{task}.html"
    cmd = [V, "explore", GOAL, "--app-id", url, "-p", "web", "--dry-run", "--json", "--no-demos",
           "--no-gates", "--replay", "0", "--values-from-screen", "--values-from-screenshots",
           "--screenshots", os.environ.get("SCREENSHOTS", "always"), "--max-steps", "15",
           "--timeout", "300", "--verify", "off", "--price", os.environ.get("PRICE", "2,10")]
    env = dict(os.environ, VELOCITY_EVENTS_FILE=str(d / "events.jsonl"))
    t = time.time()
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    p = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=600, cwd=d)
    secs = round(time.time() - t, 1)
    (d / "stderr.log").write_text(p.stderr)
    (d / "report.json").write_text(p.stdout)
    time.sleep(0.5)  # the page's reward request
    with LOCK:
        rw = REWARDS.get(task)
    try:
        rep = json.loads(p.stdout).get("report", {})
    except json.JSONDecodeError:
        rep = {}
    return {
        "task": task, "raw_reward": rw["raw"] if rw else None, "reason": rw["reason"] if rw else None,
        "success": bool(rw and rw["raw"] > 0), "ended": rw is not None,
        "outcome": rep.get("outcome"), "turns": rep.get("modelCalls"), "cost_usd": rep.get("costUsd"),
        "seconds": secs, "exit": p.returncode,
    }


def summary(out):
    rows = [json.loads(l) for l in open(out / "results.jsonl")]
    n = len(rows)
    s = sum(r["success"] for r in rows)
    mean = sum((r["raw_reward"] or 0) for r in rows) / n if n else 0
    costs = [r["cost_usd"] for r in rows if r.get("cost_usd") is not None]
    print(f"MiniWoB++ x Velocity: {s}/{n} tasks solved = {100*s/n:.1f}%; mean raw reward {mean:.3f}; "
          f"episodes ended {sum(r['ended'] for r in rows)}/{n}; est. cost ${sum(costs):.2f}")
    for r in rows:
        if not r["success"]:
            print(f"  fail {r['task']}: raw={r['raw_reward']} reason={r['reason']!r} outcome={r['outcome']}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path)
    ap.add_argument("--tasks")
    ap.add_argument("--limit", type=int)
    ap.add_argument("--summary", type=Path)
    a = ap.parse_args()
    if a.summary:
        summary(a.summary)
        return
    a.out.mkdir(parents=True, exist_ok=True)
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    tasks = a.tasks.split(",") if a.tasks else tasks_all()
    if a.limit:
        tasks = tasks[: a.limit]
    done = set()
    res = a.out / "results.jsonl"
    if res.exists():
        done = {json.loads(l)["task"] for l in open(res)}
    for t in tasks:
        if t in done:
            continue
        row = run_task(t, a.out)
        with open(res, "a") as f:
            f.write(json.dumps(row) + "\n")
        print(json.dumps({k: row[k] for k in ("task", "success", "raw_reward", "turns", "seconds")}),
              flush=True)
    summary(a.out)


if __name__ == "__main__":
    main()
