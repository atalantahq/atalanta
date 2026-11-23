# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

"""Harness sanity check: AndroidWorld's own suite runner with a no-op agent.

Proves initialize -> success check -> teardown works on this machine
without Atalanta and without a model. The agent does nothing and reports
done at once, so a task succeeds only if its start state already meets
the goal. This is a plumbing check, never a score.

  python harness_check.py --tasks ClockStopWatchPausedVerify,SystemWifiTurnOn
"""

from __future__ import annotations

import argparse
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import aw_env  # pylint: disable=g-import-not-at-top


# Goal prefix -> adb shell command that reaches it (harness check only).
ORACLE = {
    'turn wifi on': ('svc', 'wifi', 'enable'),
    'turn wifi off': ('svc', 'wifi', 'disable'),
}


def main() -> int:
  ap = argparse.ArgumentParser(description=__doc__)
  ap.add_argument('--tasks', default='ClockStopWatchPausedVerify,SystemWifiTurnOn,ContactsAddContact')
  ap.add_argument('--console-port', type=int, default=5556)
  ap.add_argument('--grpc-port', type=int, default=8556)
  ap.add_argument('--seed', type=int, default=30)
  ap.add_argument(
      '--oracle',
      action='store_true',
      help='solve the tasks in ORACLE by adb (proves the success path)',
  )
  a = ap.parse_args()

  # pylint: disable=g-import-not-at-top
  from android_world import checkpointer as checkpointer_lib
  from android_world import registry
  from android_world import suite_utils
  from android_world.agents import base_agent

  device = aw_env.Device(a.console_port, a.grpc_port)

  class NoopAgent(base_agent.EnvironmentInteractingAgent):
    """Reports done at once; with --oracle, first solves known goals by adb."""

    def step(self, goal: str) -> base_agent.AgentInteractionResult:
      if a.oracle:
        for prefix, cmd in ORACLE.items():
          if goal.lower().startswith(prefix):
            device.adb('shell', *cmd)
      return base_agent.AgentInteractionResult(done=True, data={})

  env = aw_env.load_env(device)
  try:
    reg = registry.TaskRegistry().get_registry(family='android_world')
    suite = suite_utils.create_suite(
        reg, n_task_combinations=1, seed=a.seed, tasks=a.tasks.split(',')
    )
    suite.suite_family = 'android_world'
    agent = NoopAgent(env, name='noop')
    out = tempfile.mkdtemp(prefix='aw-harness-check-')
    results = suite_utils.run(
        suite, agent, checkpointer=checkpointer_lib.IncrementalCheckpointer(out)
    )
    for r in results:
      print(
          'HARNESS',
          r['task_template'],
          'success=%s' % r['is_successful'],
          'exception=%s' % (r.get('exception_info') is not None),
          'seconds=%.1f' % r['run_time'],
      )
  finally:
    env.close()
  return 0


if __name__ == '__main__':
  sys.exit(main())
