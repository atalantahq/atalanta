"""Tests for score.py: python3 -I bench/killtest/test_score.py"""

import json
import tempfile
import unittest
from pathlib import Path

import importlib.util

spec = importlib.util.spec_from_file_location("score", Path(__file__).with_name("score.py"))
score_mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(score_mod)

PREREG = {
    "frozen": True,
    "cost_cap_usd_per_goal": 0.10,
    "thresholds": {"s0_min": 0.5, "s7_min": 0.4, "s_rel_min": 0.3, "login_secrecy": 1.0},
    "goals_min": 4,
    "login_goals_min": 2,
}


def runs(base, leak=False, trusted=(True, True, True, False)):
    out = []
    for i, t in enumerate(trusted):
        g = f"g{i}"
        tr = base / f"{g}.jsonl"
        tr.write_text("planned tap\n" + ("hunter2" if leak and i == 0 else "") + "\n")
        out.append({"goal": g, "login": i < 2, "phase": "day0", "trusted": t, "replays": 5,
                    "passed": 5, "cost_usd": 0.02 * (i + 1), "turns": 5 + i, "transcripts": [tr.name]})
        out.append({"goal": g, "phase": "day7", "replays": 5, "passed": 5 if i < 2 else 4})
        out.append({"goal": g, "phase": "release", "replays": 5, "passed": 5 if i < 2 else 3})
    return out


class ScoreTest(unittest.TestCase):
    def test_go_when_every_threshold_holds(self):
        with tempfile.TemporaryDirectory() as d:
            r = score_mod.score(runs(Path(d)), PREREG, ["hunter2"], Path(d))
        self.assertEqual(r["verdict"], "GO", r)
        self.assertEqual(r["metrics"]["s0"], 0.75)
        self.assertEqual(r["metrics"]["s7"], 0.5)
        self.assertEqual(r["metrics"]["s_rel"], 0.5)
        self.assertEqual(r["metrics"]["median_cost_usd"], 0.05)
        self.assertEqual(r["metrics"]["login_secrecy"], 1.0)

    def test_one_leaked_secret_is_no_go(self):
        with tempfile.TemporaryDirectory() as d:
            r = score_mod.score(runs(Path(d), leak=True), PREREG, ["hunter2"], Path(d))
        self.assertEqual(r["metrics"]["login_secrecy"], 0.5)
        self.assertEqual(r["verdict"], "NO-GO")

    def test_an_unfrozen_prereg_never_says_go(self):
        with tempfile.TemporaryDirectory() as d:
            r = score_mod.score(runs(Path(d)), {**PREREG, "frozen": False}, [], Path(d))
        self.assertEqual(r["verdict"], "NO-GO")
        self.assertIn("not frozen", r["problems"][0])


if __name__ == "__main__":
    unittest.main()
