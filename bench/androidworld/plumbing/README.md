# Plumbing plans (NOT a score)

Scripted `velocity explore --script` plans that solve three AndroidWorld tasks
at seed 30, instance 0, with no model. They prove the loop (AndroidWorld
initialize -> Velocity observe/ground/act -> AndroidWorld success check ->
teardown) end to end. A hand-written plan per task is not an agent: never
report these runs as an AndroidWorld result.

    python ../run.py run --out /tmp/aw-plumbing --script-dir . \
      --tasks ClockStopWatchRunning,ContactsAddContact,SystemWifiTurnOn
