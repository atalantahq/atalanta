# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

"""Run AndroidWorld with Atalanta as the agent, resumably, and score it.

  python run.py dry-run   [--model anthropic:claude-sonnet-5-5]
  python run.py run       --out RUN_DIR --model provider:model [--tasks A,B] ...
  python run.py run       --out RUN_DIR --script-dir plans/   (plumbing only)
  python run.py summarize --out RUN_DIR

Scoring follows AndroidWorld's own code (suite_utils, checked against the
harness commit recorded in each run):

- a task instance counts as solved only when the agent said it was done AND
  the task's own success check on the device passes
  (`agent_successful = task_successful if done else 0`);
- step budget per task is AndroidWorld's `int(10 * complexity)`; here one
  Atalanta planning turn counts as one step;
- task params come from `create_suite(seed=30, n_task_combinations=1)` by
  default, AndroidWorld's run.py defaults;
- the headline rate is the mean over task templates of each template's mean
  success, with instances that raised an exception left out of the tally,
  as `process_episodes` does. A strict rate (exceptions count as failures)
  is reported next to it.

Each finished instance is appended to RUN_DIR/results.jsonl at once; a rerun
with the same --out skips what is already there (resume). Model keys are
read from the environment and never written.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import datetime
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time
import traceback
from typing import Any

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import aw_env  # pylint: disable=g-import-not-at-top

# Provider prefix -> the variables that hold its key (names only; see
# crates/atalanta-nl/src/provider.rs).
KEY_ENVS = {
    'anthropic': ['ANTHROPIC_API_KEY'], 'claude': ['ANTHROPIC_API_KEY'],
    'openai': ['OPENAI_API_KEY'], 'azure-openai': ['AZURE_OPENAI_API_KEY'],
    'gemini': ['GEMINI_API_KEY', 'GOOGLE_API_KEY'],
    'openrouter': ['OPENROUTER_API_KEY'], 'groq': ['GROQ_API_KEY'],
    'mistral': ['MISTRAL_API_KEY'], 'deepseek': ['DEEPSEEK_API_KEY'],
    'xai': ['XAI_API_KEY'], 'together': ['TOGETHER_API_KEY'],
    'fireworks': ['FIREWORKS_API_KEY'], 'dashscope': ['DASHSCOPE_API_KEY'],
    'perplexity': ['PERPLEXITY_API_KEY'],
    'bedrock': ['AWS_BEARER_TOKEN_BEDROCK', 'AWS_ACCESS_KEY_ID'],
    'vertex': [], 'ollama': [], 'lmstudio': [], 'vllm': [],
    'openai-compatible': ['ATALANTA_NL_API_KEY'],
}

# List prices, dollars per million input,output tokens, for the cost
# estimate when --price is not given. Source: Anthropic's model table as
# cached 2026-10-06 (claude-api reference). Atalanta bills cache reads at a
# tenth of the input price and cache writes at 1.25 times it from the token
# counts it records; pass --price IN,OUT,CACHE_READ,CACHE_WRITE for other
# rates. The other harnesses (iosworld, miniwob, pilot) default to the same
# Sonnet price, so their costs compare.
PRICES = {
    'claude-opus-5-5': (4.0, 20.0),
    'claude-sonnet-5-5': (2.0, 10.0),
    'claude-haiku-5-5': (0.10, 0.50),
}

LAUNCHER_FALLBACK = 'com.google.android.apps.nexuslauncher'


def _git_sha(path: pathlib.Path | None) -> str | None:
  if not path:
    return None
  try:
    return subprocess.run(
        ['git', '-C', str(path), 'rev-parse', 'HEAD'],
        capture_output=True, text=True, check=True, timeout=10,
    ).stdout.strip()
  except (OSError, subprocess.SubprocessError):
    return None


def _atalanta_version(atalanta: str) -> str | None:
  try:
    return subprocess.run(
        [atalanta, '--version'], capture_output=True, text=True, timeout=20, check=True
    ).stdout.strip()
  except (OSError, subprocess.SubprocessError):
    return None


def _sha256(path: str) -> str | None:
  import hashlib  # pylint: disable=g-import-not-at-top
  p = shutil.which(path) or path
  try:
    with open(p, 'rb') as f:
      return hashlib.sha256(f.read()).hexdigest()
  except OSError:
    return None


def _default_atalanta() -> str:
  for p in (
      os.environ.get('ATALANTA_BIN'),
      shutil.which('atalanta'),
  ):
    if p and os.path.exists(p):
      return p
  return 'atalanta'


def _harness_dir(arg: str | None) -> pathlib.Path | None:
  if arg:
    return pathlib.Path(arg)
  guess = HERE.parents[2] / 'ext' / 'android_world'
  return guess if guess.exists() else None


def _launcher(device: aw_env.Device) -> str:
  out = device.adb(
      'shell', 'cmd', 'package', 'resolve-activity', '--brief',
      '-a', 'android.intent.action.MAIN', '-c', 'android.intent.category.HOME',
  )
  last = out.splitlines()[-1].strip() if out else ''
  return last.split('/')[0] if '/' in last else LAUNCHER_FALLBACK


def _installed(device: aw_env.Device) -> set[str]:
  out = device.adb('shell', 'pm', 'list', 'packages')
  return {l.split(':', 1)[1].strip() for l in out.splitlines() if ':' in l}


def _price_for(model: str | None, price: str | None) -> str | None:
  if price:
    return price
  if model:
    name = model.split(':', 1)[-1]
    if name in PRICES:
      i, o = PRICES[name]
      return f'{i},{o}'
  return None


# ---------------------------------------------------------------- dry run


def cmd_dry_run(a: argparse.Namespace) -> int:
  ok = True

  def report(name: str, good: bool, detail: str) -> None:
    nonlocal ok
    ok = ok and good
    print(f'  [{"ok" if good else "FAIL"}] {name}: {detail}')

  print('AndroidWorld x Atalanta dry run (nothing is run on the device)')
  atalanta = a.atalanta
  v = _atalanta_version(atalanta)
  report('atalanta', v is not None, v or f'{atalanta} not runnable (set --atalanta or ATALANTA_BIN)')
  device = aw_env.Device(a.console_port, a.grpc_port)
  report('adb', os.path.exists(device.adb_path) or shutil.which(device.adb_path) is not None, device.adb_path)
  booted = device.adb('shell', 'getprop', 'sys.boot_completed') == '1'
  report('device', booted, f'{device.serial} ' + ('booted' if booted else 'not booted / not found'))
  if booted:
    api = device.adb('shell', 'getprop', 'ro.build.version.sdk')
    report('API level', api == '33', f'{api} (AndroidWorld is built for 33)')
    size = device.adb('shell', 'wm', 'size')
    report('screen', '1080x2400' in size, size.replace('\n', ' '))
    pk = _installed(device)
    need = ['com.simplemobiletools.calendar.pro', 'net.gsantner.markor', 'org.tasks',
            'com.arduia.expense', 'com.google.androidenv.accessibilityforwarder']
    missing = [p for p in need if p not in pk]
    report('app setup', not missing, 'done' if not missing else f'missing {missing}: run with --perform-setup')
    report('launcher', True, _launcher(device))
  try:
    from android_world import registry  # pylint: disable=g-import-not-at-top
    n = len(registry.TaskRegistry().get_registry(family='android_world'))
    report('android_world', n == 116, f'{n} tasks; harness {_git_sha(_harness_dir(a.harness_dir)) or "sha unknown"}')
  except Exception as e:  # pylint: disable=broad-exception-caught
    report('android_world', False, f'import failed: {e}')
  grpc = subprocess.run(['lsof', '-nP', f'-iTCP:{a.grpc_port}', '-sTCP:LISTEN'],
                        capture_output=True, text=True, check=False).stdout
  report('emulator gRPC', bool(grpc.strip()), f'port {a.grpc_port} ' + ('listening' if grpc.strip() else 'not listening (launch with -grpc)'))
  if a.script_dir:
    report('model', True, f'scripted plans in {a.script_dir} (plumbing, no model)')
  elif a.model:
    provider = a.model.split(':', 1)[0]
    names = KEY_ENVS.get(provider)
    if names is None:
      report('model', False, f'unknown provider {provider!r}')
    elif not names:
      report('model', True, f'{a.model} (no key needed)')
    else:
      present = [n for n in names if os.environ.get(n)]
      report('API key', bool(present),
             f'{"/".join(names)} ' + ('set' if present else 'NOT set'))
    report('price', _price_for(a.model, a.price) is not None,
           _price_for(a.model, a.price) or 'unknown: pass --price IN,OUT for cost')
  else:
    report('model', False, 'pass --model provider:model (or --script-dir for plumbing)')
  print('ready' if ok else 'NOT ready')
  return 0 if ok else 1


# ---------------------------------------------------------------- run


def _task_app(task: Any) -> str:
  names = [n for n in getattr(task, 'app_names', ()) if n and n != 'clipper']
  if not names:
    return 'system'
  return names[0] if len(names) == 1 else 'multi-app'


def _load_done(path: pathlib.Path) -> dict[str, dict[str, Any]]:
  """The latest row per instance (a rerun of an exception supersedes it)."""
  done: dict[str, dict[str, Any]] = {}
  if path.exists():
    for line in path.read_text().splitlines():
      if line.strip():
        r = json.loads(line)
        done[r['key']] = r
  return done


def _finished(done: dict[str, dict[str, Any]]) -> set[str]:
  """Instances a resume skips: as AndroidWorld's checkpointer, an instance
  that ended in an exception is run again."""
  return {k for k, r in done.items() if not r['exception']}


def cmd_run(a: argparse.Namespace) -> int:
  # pylint: disable=g-import-not-at-top
  from android_world import constants
  from android_world import episode_runner
  from android_world import registry
  from android_world import suite_utils
  from atalanta_agent import AtalantaAgent, AtalantaConfig, app_list

  if not a.model and not a.script_dir:
    print('pass --model provider:model, or --script-dir for a plumbing run', file=sys.stderr)
    return 2
  out = pathlib.Path(a.out).resolve()
  out.mkdir(parents=True, exist_ok=True)
  lock = _lock_run_dir(out)
  if lock is None:
    return 2
  results_path = out / 'results.jsonl'
  done = _load_done(results_path)

  device = aw_env.Device(a.console_port, a.grpc_port)
  env = aw_env.load_env(device, emulator_setup=a.perform_setup)
  meta_path = out / 'run.json'
  launcher = _launcher(device)
  apps = app_list(_installed(device))
  apps_file = out / 'apps.json'
  apps_file.write_text(json.dumps(apps, indent=1))
  harness = _harness_dir(a.harness_dir)
  meta = {
      'started': datetime.datetime.now().isoformat(timespec='seconds'),
      'mode': 'plumbing (scripted plans, NOT a score)' if a.script_dir else 'model',
      'model': a.model,
      'provider': a.model.split(':', 1)[0] if a.model else None,
      'price_per_mtok': _price_for(a.model, a.price),
      'atalanta': _atalanta_version(a.atalanta),
      'atalanta_sha256': _sha256(a.atalanta),
      'harness_sha': _git_sha(harness),
      'suite_family': a.family,
      'seed': a.seed,
      'n_task_combinations': a.n_task_combinations,
      'fixed_task_seed': a.fixed_task_seed,
      'tasks': a.tasks,
      'verify': a.verify,
      'screenshots': a.screenshots or 'always (benchmark default)',
      'effort': os.environ.get('ATALANTA_NL_EFFORT', 'low (default)'),
      'device': device.serial,
      'launcher': launcher,
  }
  if meta_path.exists():
    old = json.loads(meta_path.read_text())
    for k in ('model', 'seed', 'n_task_combinations', 'mode', 'atalanta_sha256'):
      if old.get(k) != meta.get(k):
        print(f'{meta_path}: {k} was {old.get(k)!r}, now {meta.get(k)!r}; use another --out', file=sys.stderr)
        env.close()
        return 2
    meta['resumed'] = meta['started']
    meta['started'] = old.get('started')
  cfg = AtalantaConfig(
      atalanta=a.atalanta, device=device.serial, launcher=launcher,
      apps_file=str(apps_file), out_dir=out, model=a.model,
      script_dir=pathlib.Path(a.script_dir).resolve() if a.script_dir else None,
      price=_price_for(a.model, a.price), verify=a.verify,
      screenshots=a.screenshots, adb_path=device.adb_path,
  )
  if a.family.startswith('miniwob'):
    # AndroidWorld's MiniWoB protocol ends the episode when the task's
    # episode terminates (suite_utils.run's termination_fn); the app then
    # starts the next one, so actions after that spoiled the reward.
    from android_world.task_evals.miniwob import miniwob_base  # pylint: disable=g-import-not-at-top
    cfg = dataclasses.replace(cfg, stop_when=lambda: miniwob_base.is_episode_terminated(env))
  agent = AtalantaAgent(env, cfg)
  agent.transition_pause = None

  if a.family.startswith('miniwob'):
    _patch_miniwob_reward()
  reg = registry.TaskRegistry().get_registry(family=a.family)
  suite = suite_utils.create_suite(
      reg, n_task_combinations=a.n_task_combinations, seed=a.seed,
      tasks=a.tasks.split(',') if a.tasks else None,
      use_identical_params=a.fixed_task_seed,
  )
  suite.suite_family = a.family
  meta_by_task = suite_utils._extract_task_metadata()  # pylint: disable=protected-access
  spent = 0.0
  if results_path.exists():  # every attempt costs, superseded ones too
    for line in results_path.read_text().splitlines():
      if line.strip():
        spent += json.loads(line).get('cost_usd') or 0.0
  finished = _finished(done)
  exceptions_in_a_row = 0

  def run_episode(task):
    # As suite_utils.run's closure, without MiniWoB's termination.
    return episode_runner.run_episode(
        goal=task.goal, agent=agent,
        max_n_steps=suite_utils._allocate_step_budget(task.complexity),  # pylint: disable=protected-access
        start_on_home_screen=task.start_on_home_screen,
    )

  ran = 0
  try:
    for name, instances in suite.items():
      for i, task in enumerate(instances):
        key = f'{name}#{i}'
        if key in finished:
          continue
        if a.max_instances is not None and ran >= a.max_instances:
          print(f'stopping after {ran} instances (--max-instances); rerun to continue')
          return 4
        ran += 1
        if a.max_cost is not None and spent >= a.max_cost:
          print(f'stopping: estimated spend ${spent:.2f} reached --max-cost {a.max_cost}')
          return 3
        row = _run_instance(suite_utils, constants, run_episode, agent, env, task, key, a)
        row.update({
            'task': name, 'instance': i,
            'difficulty': meta_by_task.loc[name, 'difficulty'] if name in meta_by_task.index else None,
            'tags': list(meta_by_task.loc[name, 'tags']) if name in meta_by_task.index else [],
            'app': _task_app(task),
            'model': a.model or 'scripted',
            'prompt_version': row.get('prompt_version'),
        })
        spent += row.get('cost_usd') or 0.0
        with results_path.open('a') as f:
          f.write(json.dumps(row, default=str) + '\n')
        exceptions_in_a_row = exceptions_in_a_row + 1 if row['exception'] else 0
        print(f'{key}: success={row["success"]} outcome={row["outcome"]} '
              f'steps={row["steps"]}/{row["budget_steps"]} {row["wall_seconds"]:.0f}s '
              f'cost=${row.get("cost_usd") or 0:.4f} total=${spent:.2f}', flush=True)
        if exceptions_in_a_row >= a.max_exceptions_in_a_row:
          print(f'stopping: {exceptions_in_a_row} exceptions in a row (model API or device down?); '
                'rerun to resume (exceptions are run again)')
          return 5
  finally:
    meta['finished'] = datetime.datetime.now().isoformat(timespec='seconds')
    meta_path.write_text(json.dumps(meta, indent=1))
    env.close()
  summarize(out)
  return 0


def _lock_run_dir(out: pathlib.Path):
  """An exclusive lock on OUT/run.lock for this process's lifetime, or None
  (with a message) when another runner holds it: two runners on one run
  directory would interleave device actions and results."""
  import fcntl  # pylint: disable=g-import-not-at-top

  f = open(out / 'run.lock', 'a+')  # pylint: disable=consider-using-with
  try:
    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
  except OSError:
    f.seek(0)
    print(f'{out} is in use by another runner ({f.read().strip() or "pid unknown"}); not starting',
          file=sys.stderr)
    f.close()
    return None
  f.seek(0)
  f.truncate()
  f.write(f'pid {os.getpid()} since {datetime.datetime.now().isoformat(timespec="seconds")}\n')
  f.flush()
  return f  # kept open: the lock lasts until the process exits


def _patch_miniwob_reward() -> None:
  """DEVIATION (MiniWoB families only): AndroidWorld's reader parses the
  MiniWoB app's reward with `float(int(reward))`, but this app reports
  MiniWoB's time-decayed reward ("0.98886497987503"), so a solved task
  raised ValueError and was scored as a harness exception. Parse it as a
  number and count a positive reward as 1.0: MiniWoB's own convention (the
  raw reward is positive exactly when the task was done right; the decay
  only rewards speed), as `bench/miniwob/` scores the web suite."""
  from android_world.task_evals.miniwob import miniwob_base  # pylint: disable=g-import-not-at-top

  def get_episode_reward(env):
    reward = miniwob_base._extract_data(f'{miniwob_base._APP_NAME}.app.GET_REWARD_ACTION', env)  # pylint: disable=protected-access
    if reward is None:
      raise ValueError('Could not extract reward; something went wrong.')
    if not reward:
      return 0.0
    value = float(reward)
    return 1.0 if value > 0 else value

  miniwob_base.get_episode_reward = get_episode_reward


def _run_instance(suite_utils, constants, run_episode, agent, env, task, key, a) -> dict[str, Any]:
  """One instance through AndroidWorld's own `_run_task` (optionally retried
  when the task's *initialization* raised before the agent acted)."""
  E = constants.EpisodeConstants
  attempts = 0
  while True:
    attempts += 1
    agent.begin(key)
    started = time.time()
    # AndroidWorld's runner has no wall-clock limit: a hung adb or
    # accessibility call in a task's setup, success check or teardown blocks
    # it for good. The watchdog turns such a hang into an exception, the
    # class AndroidWorld already uses for harness errors (left out of the
    # tally, run again on resume). The agent's own episode is bounded by
    # Atalanta's --timeout (scored as a failure, not an exception), so the
    # watchdog allows that plus the kill margin plus --harness-timeout.
    budget = suite_utils._allocate_step_budget(task.complexity)  # pylint: disable=protected-access
    limit = (max(agent.cfg.min_timeout, budget * agent.cfg.seconds_per_step)
             + 180 + a.harness_timeout)
    try:
      with _watchdog(limit):
        episode = suite_utils._run_task(task, run_episode, env, demo_mode=False)  # pylint: disable=protected-access
    except Exception:  # pylint: disable=broad-exception-caught
      # A hang (watchdog) or an error in teardown, which `_run_task` runs
      # outside its own try.
      phase = 'before the agent acted' if agent.info.outcome == 'notRun' else 'after the episode (success check or teardown)'
      episode = {E.EXCEPTION_INFO: f'harness error {phase}:\n{traceback.format_exc()}'}
    wall = time.time() - started
    info = agent.info
    exc = episode.get(E.EXCEPTION_INFO)
    if exc is not None and info.outcome == 'notRun' and attempts <= a.init_retries:
      print(f'{key}: task initialization raised before the agent ran; retrying once', flush=True)
      task = suite_utils._instantiate_task(type(task), params=dict(task.params))  # pylint: disable=protected-access
      continue
    break
  success = episode.get(E.IS_SUCCESSFUL)
  is_exc = exc is not None
  # A MiniWoB task reads its goal from the device and forgets it at
  # teardown: use the goal the agent was given.
  try:
    goal = task.goal
  except ValueError:
    goal = getattr(agent, 'last_goal', None) or ''
  return {
      'key': key,
      'goal': goal,
      'seed': task.params.get('seed'),
      'success': None if is_exc else float(success),
      'exception': is_exc,
      'exception_info': (exc or '')[-1500:] if is_exc else None,
      # A MiniWoB episode the task itself ended counts as done, as
      # suite_utils.run's termination_fn ends it.
      'agent_done': info.outcome in ('success', 'unverified', 'gaveUp') or info.terminated_by_task,
      'terminated_by_task': info.terminated_by_task,
      'outcome': info.outcome,
      'reason': info.reason,
      'answer': info.answer,
      'steps': info.turns,
      'actions': info.actions,
      'budget_steps': info.budget_steps,
      'wall_seconds': round(wall, 1),
      'atalanta_seconds': round(info.atalanta_seconds, 1),
      'model_calls': info.model_calls,
      'input_tokens': info.input_tokens,
      'output_tokens': info.output_tokens,
      'cost_usd': info.cost_usd,
      'error': info.error,
      'exit_code': info.exit_code,
      'events_file': info.events_file,
      'prompt_version': info.prompt_version,
      'attempts': attempts,
  }


class HarnessTimeout(Exception):
  """An instance ran past its wall-clock limit."""


class _watchdog:  # pylint: disable=invalid-name
  """Raise HarnessTimeout in the main thread after `seconds` (SIGALRM).

  The handler runs between Python bytecodes, so it interrupts sleeps,
  retry loops and subprocess waits (a running `atalanta` child is killed by
  `subprocess.run` as the exception unwinds)."""

  def __init__(self, seconds: float):
    self.seconds = seconds

  def __enter__(self):
    import signal  # pylint: disable=g-import-not-at-top

    def fire(signum, frame):
      del signum, frame
      raise HarnessTimeout(f'instance ran past its {self.seconds:.0f} s wall-clock limit')

    self._old = signal.signal(signal.SIGALRM, fire)
    signal.setitimer(signal.ITIMER_REAL, self.seconds)
    return self

  def __exit__(self, *exc):
    import signal  # pylint: disable=g-import-not-at-top
    signal.setitimer(signal.ITIMER_REAL, 0)
    signal.signal(signal.SIGALRM, self._old)
    return False


# ---------------------------------------------------------------- summary


def summarize(out: pathlib.Path) -> dict[str, Any]:
  rows = list(_load_done(out / 'results.jsonl').values())
  meta_family = (json.loads((out / 'run.json').read_text()).get('suite_family')
                 if (out / 'run.json').exists() else None) or 'android_world'
  # In a MiniWoB family the harness stops the agent when the task's episode
  # terminates (outcome "stopped"): that is the episode's end, as in
  # AndroidWorld's own runner, so it counts as done.
  if meta_family.startswith('miniwob'):
    for r in rows:
      if r.get('outcome') == 'stopped':
        r['agent_done'] = True
  # AndroidWorld credits a task only when the agent said done
  # (`suite_utils._run_task`). Rows written before the adapter repeated the
  # agent's verdict on a second step() can carry a credit without it.
  for r in rows:
    if r.get('agent_done') is False and r.get('success'):
      r['success'] = 0.0
  meta = json.loads((out / 'run.json').read_text()) if (out / 'run.json').exists() else {}
  by_task: dict[str, list[dict[str, Any]]] = collections.defaultdict(list)
  for r in rows:
    by_task[r['task']].append(r)

  def official(rs: list[dict[str, Any]]) -> tuple[float | None, int]:
    # process_episodes: per-template mean over non-exception instances,
    # then the mean of the templates that have one.
    per = collections.defaultdict(list)
    for r in rs:
      if not r['exception']:
        per[r['task']].append(r['success'])
    means = [sum(v) / len(v) for v in per.values() if v]
    return (sum(means) / len(means) if means else None), len(means)

  def strict(rs: list[dict[str, Any]]) -> float | None:
    return sum((r['success'] or 0.0) for r in rs) / len(rs) if rs else None

  def group(keyf) -> dict[str, Any]:
    g = collections.defaultdict(list)
    for r in rows:
      g[keyf(r)].append(r)
    return {
        k: {'official': official(v)[0], 'strict': strict(v), 'instances': len(v)}
        for k, v in sorted(g.items())
    }

  ran = [r for r in rows if r['outcome'] not in ('notRun',)]
  steps = sum(r['steps'] for r in ran)
  # Runs recorded before the rename from Velocity carry the old key.
  vel_s = sum(r.get('atalanta_seconds', r.get('velocity_seconds')) for r in ran)
  rate, n_templates = official(rows)
  s = {
      'meta': meta,
      'note': ('PLUMBING RUN with scripted plans: not a score.'
               if meta.get('mode', '').startswith('plumbing') else
               'Self-run; AndroidWorld leaderboard numbers are self-reported.'),
      'instances': len(rows),
      'templates': len(by_task),
      'exceptions': sum(r['exception'] for r in rows),
      'success_rate_official': rate,
      'templates_scored': n_templates,
      'success_rate_strict': strict(rows),
      'by_difficulty': group(lambda r: r.get('difficulty') or '?'),
      'by_app': group(lambda r: r.get('app') or '?'),
      'by_outcome': dict(collections.Counter(r['outcome'] for r in rows)),
      'mean_steps': steps / len(ran) if ran else None,
      'mean_seconds_per_step': vel_s / steps if steps else None,
      'mean_wall_seconds': sum(r['wall_seconds'] for r in rows) / len(rows) if rows else None,
      'input_tokens': sum(r['input_tokens'] for r in rows),
      'output_tokens': sum(r['output_tokens'] for r in rows),
      'model_calls': sum(r['model_calls'] for r in rows),
      'total_cost_usd': sum((r['cost_usd'] or 0.0) for r in rows),
  }
  (out / 'summary.json').write_text(json.dumps(s, indent=1, default=str))
  pct = lambda x: '-' if x is None else f'{100 * x:.1f}%'
  lines = [
      f'# AndroidWorld x Atalanta: {meta.get("model") or "scripted"}',
      '', s['note'], '',
      f'- success (AndroidWorld convention, exceptions excluded): **{pct(rate)}** over {n_templates} templates',
      f'- success (strict, exceptions = failure): {pct(s["success_rate_strict"])} over {len(rows)} instances',
      f'- exceptions: {s["exceptions"]}; outcomes: {s["by_outcome"]}',
      f'- mean steps {s["mean_steps"] and round(s["mean_steps"], 1)}, '
      f'mean s/step {s["mean_seconds_per_step"] and round(s["mean_seconds_per_step"], 1)}, '
      f'tokens {s["input_tokens"]}/{s["output_tokens"]}, est. cost ${s["total_cost_usd"]:.2f}',
      f'- atalanta {meta.get("atalanta") or meta.get("velocity")}, harness {meta.get("harness_sha")}, seed {meta.get("seed")}, '
      f'combinations {meta.get("n_task_combinations")}, verify {meta.get("verify")}',
      '', '| difficulty | official | strict | n |', '|---|---|---|---|',
  ]
  for k, v in s['by_difficulty'].items():
    lines.append(f'| {k} | {pct(v["official"])} | {pct(v["strict"])} | {v["instances"]} |')
  lines += ['', '| app | official | strict | n |', '|---|---|---|---|']
  for k, v in s['by_app'].items():
    lines.append(f'| {k} | {pct(v["official"])} | {pct(v["strict"])} | {v["instances"]} |')
  lines += ['', '| task | success | outcome | steps | s | cost | note |', '|---|---|---|---|---|---|---|']
  for r in sorted(rows, key=lambda r: r['key']):
    note = (r.get('reason') or r.get('error') or '')[:90].replace('|', '/')
    lines.append(f'| {r["key"]} | {r["success"]} | {r["outcome"]} | {r["steps"]}/{r["budget_steps"]} | '
                 f'{r["wall_seconds"]:.0f} | {r["cost_usd"] or 0:.3f} | {note} |')
  (out / 'summary.md').write_text('\n'.join(lines) + '\n')
  print('\n'.join(lines[:8]))
  return s


def cmd_summarize(a: argparse.Namespace) -> int:
  summarize(pathlib.Path(a.out).resolve())
  return 0


def main() -> int:
  ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
  sub = ap.add_subparsers(dest='cmd', required=True)

  def common(p):
    p.add_argument('--console-port', type=int, default=5556)
    p.add_argument('--family', default='android_world',
                   help="AndroidWorld task family: android_world (116 tasks), miniwob (MobileMiniWoB++), miniwob_subset")
    p.add_argument('--grpc-port', type=int, default=8556)
    p.add_argument('--atalanta', default=_default_atalanta(), help='atalanta binary (or $ATALANTA_BIN)')
    p.add_argument('--model', help='provider:model, e.g. anthropic:claude-sonnet-5-5')
    p.add_argument('--script-dir', help='plumbing: scripted plans <Task>.json, no model')
    p.add_argument('--price', help='IN,OUT dollars per million tokens (default: list price if known)')
    p.add_argument('--harness-dir', help='android_world checkout (for its commit SHA)')

  d = sub.add_parser('dry-run', help='check env, AVD, harness and key presence')
  common(d)
  r = sub.add_parser('run', help='run (or resume) the suite')
  common(r)
  r.add_argument('--out', required=True)
  r.add_argument('--tasks', help='comma-separated task names (default: all 116)')
  r.add_argument('--n-task-combinations', type=int, default=1)
  r.add_argument('--seed', type=int, default=30, help="AndroidWorld's task_random_seed (its default: 30)")
  r.add_argument('--fixed-task-seed', action='store_true')
  r.add_argument('--verify', default='final', help='atalanta --verify (final: self-check before done)')
  r.add_argument('--screenshots', help='atalanta --screenshots (benchmark default: always)')
  r.add_argument('--max-cost', type=float, help='stop starting tasks once the estimate passes this')
  r.add_argument('--max-instances', type=int, help='run at most this many new instances, then stop (resume later)')
  r.add_argument('--max-exceptions-in-a-row', type=int, default=3)
  r.add_argument('--harness-timeout', type=float, default=600,
                 help="seconds allowed for a task's setup, success check and teardown together")
  r.add_argument('--init-retries', type=int, default=0,
                 help='retry an instance whose task initialization raised before the agent acted '
                 '(default 0, as AndroidWorld: such instances are exceptions, left out of the tally)')
  r.add_argument('--perform-setup', action='store_true', help="AndroidWorld's one-time app setup")
  s = sub.add_parser('summarize', help='rewrite summary.json / summary.md')
  s.add_argument('--out', required=True)
  a = ap.parse_args()
  if a.cmd == 'dry-run':
    return cmd_dry_run(a)
  if a.cmd == 'run':
    return cmd_run(a)
  return cmd_summarize(a)


if __name__ == '__main__':
  try:
    sys.exit(main())
  except KeyboardInterrupt:
    print('interrupted; rerun the same command to resume', file=sys.stderr)
    sys.exit(130)
  except Exception:  # pylint: disable=broad-exception-caught
    traceback.print_exc()
    sys.exit(1)
