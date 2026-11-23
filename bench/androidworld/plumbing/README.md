# Plumbing plans (not a score)

Scripted plans for `atalanta explore --script` that solve three AndroidWorld
tasks (seed 30, instance 0) with no model. Use them to check that the loop
works end to end before spending model money: AndroidWorld initialise,
Atalanta observe/ground/act, AndroidWorld success check, teardown.

A hand-written plan per task is not an agent. Never report these runs as an
AndroidWorld result.

Prerequisites are steps 1 to 3 of [../REPRODUCE.md](../REPRODUCE.md). Run the
command below from this directory, using the venv's `python`:

    python ../run.py run --out /tmp/aw-plumbing --script-dir . \
      --tasks ClockStopWatchRunning,ContactsAddContact,SystemWifiTurnOn
