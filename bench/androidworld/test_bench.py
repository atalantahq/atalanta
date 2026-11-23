"""Offline checks for the bench scripts (no device, no model).

  python -m pytest bench/androidworld/test_bench.py
"""

import json
import os
import pathlib
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import run  # pylint: disable=g-import-not-at-top
import atalanta_agent  # pylint: disable=g-import-not-at-top


def _row(task, i, success, exception=False, outcome='success', cost=0.1, steps=4):
  return {
      'key': f'{task}#{i}', 'task': task, 'instance': i,
      'success': None if exception else success, 'exception': exception,
      'outcome': outcome, 'steps': steps, 'budget_steps': 10,
      'wall_seconds': 10.0, 'atalanta_seconds': 8.0, 'input_tokens': 1000,
      'output_tokens': 100, 'model_calls': 5, 'cost_usd': cost,
      'difficulty': 'easy', 'app': 'clock', 'reason': '', 'error': None,
  }


def test_official_rate_averages_templates_and_skips_exceptions():
  with tempfile.TemporaryDirectory() as d:
    out = pathlib.Path(d)
    rows = [
        _row('A', 0, 1.0), _row('A', 1, 0.0),       # template mean 0.5
        _row('B', 0, 1.0),                          # 1.0
        _row('C', 0, None, exception=True),         # left out of the tally
    ]
    (out / 'results.jsonl').write_text('\n'.join(json.dumps(r) for r in rows) + '\n')
    (out / 'run.json').write_text(json.dumps({'mode': 'model', 'model': 'x:y'}))
    s = run.summarize(out)
    assert abs(s['success_rate_official'] - 0.75) < 1e-9
    assert s['templates_scored'] == 2
    # Strict: exceptions count as failures, over all instances.
    assert abs(s['success_rate_strict'] - 0.5) < 1e-9
    assert s['exceptions'] == 1
    assert abs(s['total_cost_usd'] - 0.4) < 1e-9
    assert (out / 'summary.md').read_text().startswith('# AndroidWorld x Atalanta')


def test_a_credit_without_the_agent_saying_done_is_not_counted():
  with tempfile.TemporaryDirectory() as d:
    out = pathlib.Path(d)
    ran_out = dict(_row('A', 0, 1.0, outcome='budgetExhausted'), agent_done=False)
    rows = [ran_out, dict(_row('B', 0, 1.0), agent_done=True)]
    (out / 'results.jsonl').write_text('\n'.join(json.dumps(r) for r in rows) + '\n')
    s = run.summarize(out)
    assert abs(s['success_rate_strict'] - 0.5) < 1e-9


def test_plumbing_runs_are_labelled():
  with tempfile.TemporaryDirectory() as d:
    out = pathlib.Path(d)
    (out / 'results.jsonl').write_text(json.dumps(_row('A', 0, 1.0)) + '\n')
    (out / 'run.json').write_text(json.dumps({'mode': 'plumbing (scripted plans, NOT a score)'}))
    s = run.summarize(out)
    assert 'not a score' in s['note']


def test_resume_reads_finished_keys():
  with tempfile.TemporaryDirectory() as d:
    p = pathlib.Path(d) / 'results.jsonl'
    p.write_text(json.dumps(_row('A', 0, 1.0)) + '\n\n' + json.dumps(_row('B', 0, 0.0)) + '\n')
    assert set(run._load_done(p)) == {'A#0', 'B#0'}  # pylint: disable=protected-access


def test_price_defaults_to_list_price_and_flag_wins():
  assert run._price_for('anthropic:claude-sonnet-5-5', None) == '2.0,10.0'  # pylint: disable=protected-access
  assert run._price_for('anthropic:claude-sonnet-5-5', '1,2') == '1,2'  # pylint: disable=protected-access
  assert run._price_for('openai:unknown', None) is None  # pylint: disable=protected-access


def test_done_outcomes_are_the_agents_own_stops():
  assert 'success' in atalanta_agent.DONE_OUTCOMES
  assert 'gaveUp' in atalanta_agent.DONE_OUTCOMES
  for not_done in ('budgetExhausted', 'timedOut', 'stuck', 'failed', 'error'):
    assert not_done not in atalanta_agent.DONE_OUTCOMES


def test_a_repeated_step_keeps_the_agents_verdict():
  # AndroidWorld calls step() again after the episode; a run that ran out of
  # turns must not turn into done=True there (it was credited 1.0 once).
  agent = object.__new__(atalanta_agent.AtalantaAgent)
  agent._ran = True  # pylint: disable=protected-access
  agent._done = False  # pylint: disable=protected-access
  assert agent.step('goal').done is False
  agent._done = True  # pylint: disable=protected-access
  assert agent.step('goal').done is True


def test_json_report_is_parsed_even_after_noise():
  assert atalanta_agent._parse_json('noise {"report": {"outcome": "success"}}')['report']['outcome'] == 'success'  # pylint: disable=protected-access
  assert atalanta_agent._parse_json('') is None  # pylint: disable=protected-access


def test_resume_runs_exceptions_again_and_the_latest_row_wins():
  with tempfile.TemporaryDirectory() as d:
    p = pathlib.Path(d) / 'results.jsonl'
    rows = [_row('A', 0, None, exception=True), _row('B', 0, 1.0), _row('A', 0, 0.0)]
    p.write_text('\n'.join(json.dumps(r) for r in rows) + '\n')
    done = run._load_done(p)  # pylint: disable=protected-access
    assert done['A#0']['success'] == 0.0
    assert run._finished(done) == {'A#0', 'B#0'}  # pylint: disable=protected-access
    p.write_text(json.dumps(_row('C', 0, None, exception=True)) + '\n')
    assert run._finished(run._load_done(p)) == set()  # pylint: disable=protected-access


def test_infra_failures_are_not_agent_outcomes():
  for o in atalanta_agent.INFRA_OUTCOMES:
    assert o not in atalanta_agent.DONE_OUTCOMES


def test_watchdog_turns_a_hang_into_an_exception():
  import time  # pylint: disable=g-import-not-at-top
  started = time.time()
  try:
    with run._watchdog(0.3):  # pylint: disable=protected-access
      time.sleep(5)
  except run.HarnessTimeout as e:
    assert 'wall-clock limit' in str(e)
  else:
    raise AssertionError('no timeout')
  assert time.time() - started < 2
  with run._watchdog(5):  # pylint: disable=protected-access
    pass  # disarmed on exit
  time.sleep(0.1)


def test_a_second_runner_cannot_take_the_run_dir():
  with tempfile.TemporaryDirectory() as d:
    out = pathlib.Path(d)
    first = run._lock_run_dir(out)  # pylint: disable=protected-access
    assert first is not None
    # flock is per open file description: a second open in this process
    # conflicts just as another process would.
    assert run._lock_run_dir(out) is None  # pylint: disable=protected-access
    first.close()
    again = run._lock_run_dir(out)  # pylint: disable=protected-access
    assert again is not None
    again.close()
