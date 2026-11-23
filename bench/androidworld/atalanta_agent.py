# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

"""AndroidWorld agent that drives the device through `atalanta explore`.

Integration: one `atalanta explore --benchmark --json` child process per
episode. AndroidWorld calls `step()` in a loop; the first call runs the
whole episode inside Atalanta (observe -> plan -> ground -> act, with
Atalanta's own step budget set to AndroidWorld's), then returns `done`.
A grounded answer (information-retrieval tasks) goes to AndroidWorld's
answer mechanism (`env.execute_action(answer)` -> `env.interaction_cache`)
before `done` is returned, exactly where the reference agents put it.

Why a child process per episode rather than a long-lived `atalanta mcp`
session: the agent loop already lives in Atalanta (`explore`), so MCP would
only add a JSON-RPC layer around the same child (`run_task` spawns
`atalanta explore --json` anyway). A process per episode gives isolation
(a crash or hung model call can't leak into the next task), a hard
wall-clock kill, and a clean per-task events file.
"""

from __future__ import annotations

import dataclasses
import json
import os
import pathlib
import subprocess
import tempfile
import time
from typing import Any

from android_world.agents import base_agent
from android_world.env import interface
from android_world.env import json_action

# Atalanta outcomes where the agent itself decided to stop (AndroidWorld's
# "done"; scored by the task's own check). Budget, time, stuck and failures
# are not "done": AndroidWorld scores those 0, as for any agent that runs out
# of steps.
DONE_OUTCOMES = ('success', 'unverified', 'gaveUp')
# Outcomes that are infrastructure failures (the model API, the device, a
# Atalanta crash), raised as exceptions rather than scored.
INFRA_OUTCOMES = ('failed', 'error')

# The AndroidWorld apps the agent may open by name (`openApp`), as the names
# AndroidWorld's own `open_app` action knows them. Harness helper apps
# (android world overlay, clipper, miniwob) are left out.
_HELPER_PACKAGES = (
    'com.example.androidworld',
    'ca.zgrs.clipper',
    'com.google.androidenv.miniwob',
    'com.google.androidenv.accessibilityforwarder',
)


def app_list(installed: set[str]) -> dict[str, str]:
  """{display name: package} for AndroidWorld's app names on this device."""
  from android_world.env import adb_utils  # pylint: disable=g-import-not-at-top

  out: dict[str, str] = {}
  for pattern, activity in adb_utils._PATTERN_TO_ACTIVITY.items():  # pylint: disable=protected-access
    pkg = activity.split('/')[0]
    if pkg not in installed or pkg in _HELPER_PACKAGES:
      continue
    name = pattern.split('|')[0].strip().rstrip(':')
    name = ' '.join(w if w.isupper() else w.capitalize() for w in name.split())
    if pkg not in out.values():
      out[name] = pkg
  return out


@dataclasses.dataclass
class AtalantaConfig:
  atalanta: str
  device: str
  launcher: str
  apps_file: str
  out_dir: pathlib.Path
  model: str | None = None
  script_dir: pathlib.Path | None = None
  price: str | None = None
  verify: str = 'final'
  screenshots: str | None = None
  seconds_per_step: float = 30.0
  min_timeout: float = 120.0
  adb_path: str | None = None
  extra_args: tuple[str, ...] = ()
  # AndroidWorld's own episode end, checked while Atalanta runs (MiniWoB:
  # the task's episode terminated). When it turns true, Atalanta is
  # stopped and the episode counts as done, as suite_utils.run's
  # termination_fn ends an agent's episode.
  stop_when: Any = None


@dataclasses.dataclass
class EpisodeInfo:
  """What one episode cost and how it ended, for results.jsonl."""

  outcome: str = 'notRun'
  reason: str = ''
  turns: int = 0
  actions: int = 0
  model_calls: int = 0
  input_tokens: int = 0
  output_tokens: int = 0
  cost_usd: float | None = None
  atalanta_seconds: float = 0.0
  answer: str | None = None
  # The run was stopped because AndroidWorld said the task's episode ended.
  terminated_by_task: bool = False
  exit_code: int | None = None
  error: str | None = None
  events_file: str | None = None
  prompt_version: str | None = None
  budget_steps: int = 0


class AtalantaAgent(base_agent.EnvironmentInteractingAgent):
  """Runs each episode as one `atalanta explore --benchmark` child."""

  def __init__(self, env: interface.AsyncEnv, cfg: AtalantaConfig, name: str = 'atalanta'):
    super().__init__(env, name=name, transition_pause=None)
    self.cfg = cfg
    self.task_key = 'task'
    self.info = EpisodeInfo()
    self._ran = False
    self._done = False

  def begin(self, task_key: str) -> None:
    """Called by the runner before each episode (names the output files)."""
    self.task_key = task_key
    self.info = EpisodeInfo()
    self._ran = False

  def reset(self, go_home: bool = False) -> None:
    super().reset(go_home)
    self._ran = False

  def _command(self, goal: str, budget: int, timeout: int) -> list[str]:
    c = self.cfg
    cmd = [
        c.atalanta, 'explore', goal,
        '--app-id', c.launcher,
        '--device', c.device,
        '--platform', 'android',
        '--benchmark',
        '--values-from-goal',
        '--values-from-screen',
        # Text read from a screenshot (a receipt, a gallery image, a video
        # frame): tasks that transcribe an image need it. Code can't check
        # pixels, so it is Atalanta's separate opt-in (2026-11-07).
        '--values-from-screenshots',
        '--apps-file', c.apps_file,
        '--max-steps', str(budget),
        '--timeout', str(timeout),
        '--verify', c.verify,
        '--lease-wait', '120',
        '--json',
    ]
    if c.screenshots:
      cmd += ['--screenshots', c.screenshots]
    if c.price:
      cmd += ['--price', c.price]
    if c.script_dir is not None:
      cmd += ['--script', str(c.script_dir / f'{self.task_key.split("#")[0]}.json')]
    elif c.model:
      cmd += ['--model', c.model]
    cmd += list(c.extra_args)
    return cmd

  def step(self, goal: str) -> base_agent.AgentInteractionResult:
    self.last_goal = goal
    if self._ran:
      # The episode already ran in full; never act twice. Repeat the
      # agent's own verdict: AndroidWorld credits a task only when the agent
      # said done (`suite_utils._run_task`), so a run that ran out of turns
      # must stay not-done here.
      return base_agent.AgentInteractionResult(done=self._done, data={})
    self._ran = True
    self._done = False
    _close_forwarder_crash_dialog(self.cfg)
    budget = int(self._max_steps or 10)
    timeout = int(max(self.cfg.min_timeout, budget * self.cfg.seconds_per_step))
    task_dir = self.cfg.out_dir / 'episodes' / self.task_key.replace('#', '_')
    task_dir.mkdir(parents=True, exist_ok=True)
    events = task_dir / 'events.jsonl'
    if events.exists():
      events.unlink()
    env = dict(os.environ)
    env['ATALANTA_EVENTS_FILE'] = str(events)
    # Keep AndroidWorld's accessibility forwarder alive: many tasks check
    # success through it, and a suppressing UiAutomation connection kills
    # its tree for the rest of the run.
    env['ATALANTA_ANDROID_KEEP_A11Y'] = '1'
    if self.cfg.adb_path:
      env['ATALANTA_ADB_PATH'] = self.cfg.adb_path
    info = self.info
    info.budget_steps = budget
    info.events_file = str(events)
    started = time.time()
    try:
      proc = _run_atalanta(self._command(goal, budget, timeout), env, task_dir,
                           timeout + 180, self.cfg.stop_when, info)
      info.exit_code = proc.returncode
      (task_dir / 'stderr.log').write_text(proc.stderr)
      report = _parse_json(proc.stdout)
      if report is None:
        info.outcome = 'error'
        info.error = f'atalanta exit {proc.returncode}: ' + proc.stderr.strip()[-600:]
      else:
        (task_dir / 'result.json').write_text(json.dumps(report, indent=1))
        rep = report.get('report', {})
        info.outcome = rep.get('outcome', 'error')
        info.reason = rep.get('reason', '')
        info.turns = rep.get('turns', 0)
        info.actions = len(rep.get('steps', []))
        info.model_calls = rep.get('modelCalls', 0)
        info.input_tokens = rep.get('inputTokens', 0)
        info.output_tokens = rep.get('outputTokens', 0)
        info.cost_usd = rep.get('costUsd')
        info.answer = rep.get('answer')
        info.prompt_version = rep.get('promptVersion')
    except subprocess.TimeoutExpired:
      info.outcome = 'timedOut'
      info.error = 'atalanta did not finish within its timeout (killed)'
    info.atalanta_seconds = time.time() - started

    if info.outcome in INFRA_OUTCOMES:
      # The model service or Atalanta itself failed, not the agent's
      # judgement: raise, as AndroidWorld's own agents do when their model
      # calls fail. `_run_task` records an exception (left out of the
      # official tally, re-run on resume).
      raise RuntimeError(f'atalanta {info.outcome}: {info.reason or info.error}')

    if info.answer:
      # AndroidWorld's answer channel (what its `answer` action does).
      self.env.execute_action(
          json_action.JSONAction(action_type=json_action.ANSWER, text=info.answer)
      )
    done = info.outcome in DONE_OUTCOMES or info.terminated_by_task
    self._done = done
    return base_agent.AgentInteractionResult(
        done=done, data={'atalanta': dataclasses.asdict(info)}
    )


def _run_atalanta(cmd, env, cwd, timeout, stop_when, info):
  """Run Atalanta; with `stop_when`, check it every second and stop Atalanta
  with SIGINT (it ends at its next turn as `stopped`, with a report) once
  it is true. Returns a CompletedProcess-like object."""
  if stop_when is None:
    return subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=timeout,
                          check=False, cwd=str(cwd))
  import signal  # pylint: disable=g-import-not-at-top
  out_f = tempfile.TemporaryFile(mode='w+')
  err_f = tempfile.TemporaryFile(mode='w+')
  proc = subprocess.Popen(cmd, stdout=out_f, stderr=err_f, text=True, env=env, cwd=str(cwd))
  deadline = time.time() + timeout
  stopped_at = None
  while proc.poll() is None:
    if time.time() > deadline:
      proc.kill()
      proc.wait()
      raise subprocess.TimeoutExpired(cmd, timeout)
    if stopped_at is None:
      try:
        if stop_when():
          info.terminated_by_task = True
          proc.send_signal(signal.SIGINT)
          stopped_at = time.time()
      except Exception:  # pylint: disable=broad-exception-caught
        pass  # a failed check is retried next second
    elif time.time() - stopped_at > 30:
      proc.kill()
    time.sleep(1.0)
  out_f.seek(0)
  err_f.seek(0)
  return subprocess.CompletedProcess(cmd, proc.returncode, out_f.read(), err_f.read())


def _close_forwarder_crash_dialog(cfg: AtalantaConfig) -> None:
  """Close the "accessibilityforwarder keeps stopping" system dialog.

  android_env's accessibility forwarder (a harness helper, not part of any
  task) can crash when the env connects, leaving a system crash dialog over
  the task's first screen. Only that dialog is closed, and only when it
  has focus; any other dialog is left to the agent.
  """
  if not cfg.adb_path:
    return
  base = [cfg.adb_path, '-s', cfg.device, 'shell']
  try:
    focus = subprocess.run(base + ['dumpsys', 'window', 'windows'], capture_output=True,
                           text=True, timeout=15, check=False).stdout
    line = next((l for l in focus.splitlines() if 'mCurrentFocus' in l), '')
    if 'Application Error' in line and 'accessibilityforwarder' in line:
      subprocess.run(base + ['input', 'keyevent', 'KEYCODE_BACK'], timeout=15, check=False)
      time.sleep(1)
  except (OSError, subprocess.SubprocessError):
    pass


def _parse_json(stdout: str) -> dict[str, Any] | None:
  s = stdout.strip()
  if not s:
    return None
  try:
    return json.loads(s)
  except json.JSONDecodeError:
    start = s.find('{')
    if start < 0:
      return None
    try:
      return json.loads(s[start:])
    except json.JSONDecodeError:
      return None
