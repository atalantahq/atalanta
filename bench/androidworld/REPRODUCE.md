# Reproducing Atalanta's AndroidWorld run

This is the procedure for re-running the AndroidWorld benchmark with Atalanta
as the agent, for anyone who wants to check how the published result was
produced. The results and caveats are in
[docs/BENCHMARKS.md](../../docs/BENCHMARKS.md#androidworld).

Everything here was run on an Apple Silicon Mac (macOS, 24 GB RAM). Steps 1 to
4 need no model key; step 5 does and spends real money (see `--max-cost`).
Commands run from the repository root unless a block starts with `cd`.

## 1. Harness

Use AndroidWorld at commit `55ddaf5edc6f6c25673aed343907d5b9dca66f84`
(google-research/android_world, 2026-10-23), with Python 3.11 in its own venv.
`ext/` is git-ignored:

```bash
git clone https://github.com/google-research/android_world ext/android_world
git -C ext/android_world checkout 55ddaf5edc6f6c25673aed343907d5b9dca66f84
uv venv --python 3.11 ext/aw-venv
VIRTUAL_ENV=$PWD/ext/aw-venv uv pip install -r ext/android_world/requirements.txt fastapi uvicorn
VIRTUAL_ENV=$PWD/ext/aw-venv uv pip install ./ext/android_world   # generates its protos
brew install ffmpeg
```

## 2. Emulator

AndroidWorld's device: Pixel 6 shape (1080x2400, 420 dpi), API 33 Google APIs
image (arm64-v8a on Apple Silicon), AVD named `AndroidWorldAvd`.

```bash
sdkmanager "system-images;android-33;google_apis;arm64-v8a"
# avdmanager create avd -n AndroidWorldAvd -k "system-images;android-33;google_apis;arm64-v8a" -d pixel_6
# (on this machine avdmanager failed with "null"; the AVD was written by hand:
#  ~/.android/avd/AndroidWorldAvd.avd/config.ini with the image above,
#  hw.lcd.width=1080, hw.lcd.height=2400, hw.lcd.density=420, hw.ramSize=4096)
emulator -avd AndroidWorldAvd -port 5556 -grpc 8556 -no-snapshot -no-window -no-audio -no-boot-anim
```

Console port 5556 and gRPC 8556 keep it apart from another emulator on
5554/8554. AndroidWorld's own `run.py` hard-codes gRPC 8554, so the scripts
here pass both ports to its `env_launcher` (`--console-port`, `--grpc-port`).

One-time app setup. AndroidWorld downloads its APKs from Google's bucket and
configures them; this takes about 4 minutes:

```bash
cd bench/androidworld
../../ext/aw-venv/bin/python run.py run --perform-setup --out /tmp/aw-setup --script-dir plumbing --tasks ClockStopWatchRunning
```

## 3. Atalanta

Build the release binary and point the scripts at it:

```bash
cargo build --release -p atalanta-cli
export ATALANTA_BIN=$PWD/target/release/atalanta
```

## 4. Check, then plumbing (no model)

```bash
cd bench/androidworld
PY=../../ext/aw-venv/bin/python
$PY run.py dry-run --model anthropic:claude-sonnet-5-5   # env, AVD, harness, key presence (names only)
$PY harness_check.py --oracle --tasks SystemWifiTurnOn,ContactsAddContact   # harness alone, no Atalanta
$PY run.py run --out runs/plumbing --script-dir plumbing \
  --tasks ClockStopWatchRunning,ContactsAddContact,SystemWifiTurnOn        # scripted plans: NOT a score
```

## 5. The scored run

The protocol is AndroidWorld's, as its `run.py` and `suite_utils` define it:

- all 116 tasks of the `android_world` family;
- `--n-task-combinations 1` and task seed 30;
- a step budget of `int(10 * complexity)` per task;
- success means the agent said done and the task's own device check passes.

```bash
export ANTHROPIC_API_KEY=...           # or the key of the provider you pass
$PY run.py run --out runs/full-sonnet-5-5 --model anthropic:claude-sonnet-5-5 --max-cost 150
```

`--max-cost` stops before starting a task once the running estimate passes it.
The estimate uses list prices and counts cache reads at the full input price,
so it is an upper bound. If a run is interrupted or stopped, rerun the same
command; finished instances are skipped. `run.py summarize --out
runs/full-sonnet-5-5` rewrites the summary.

Outputs in `--out`:

- `run.json`: model, provider, prices, Atalanta version, harness SHA, seed,
  combinations, prompt settings.
- `results.jsonl`: one line per instance with success, steps, wall time,
  tokens, cost, outcome, answer, error.
- `summary.json` and `summary.md`: overall, per difficulty, per app.
- `episodes/<task>_<i>/`: Atalanta's events JSONL trajectory, its JSON report
  and stderr.

Other providers: any `atalanta` preset works (`--model openai:…`, `gemini:…`,
and so on). Pass `--price IN,OUT` for a cost estimate when the model is not in
`run.py`'s price table.

## Resuming, and how the published run was checked

A run directory holds one runner at a time (`run.lock`). Rerunning the same
`run.py run --out DIR …` command resumes: instances with a result are
skipped, instances that ended in an exception or have no result yet run
again from their own initialisation (as AndroidWorld's checkpointer does),
and `run.py` refuses a different model, seed, combination count or Atalanta
binary (SHA-256) for an existing directory.

The published run (`full-sonnet-5-5`) was resumed three times: after a hang
in AndroidWorld's own check/teardown, after a disk disconnect and host
reboot (emulator cold-booted with the same flags, then `adb -s emulator-5556
root`), and after a second disk disconnect. Integrity was checked with:

```bash
python3 - <<'PY'
import json, collections
rows = [json.loads(l) for l in open('runs/full-sonnet-5-5/results.jsonl') if l.strip()]
latest = {r['key']: r for r in rows}                     # what is scored
print(len(rows), 'lines,', len(latest), 'instances')
spans = {}
for k, r in latest.items():
    ev = [json.loads(l) for l in open(r['events_file']) if l.strip()]
    turns = [e['turn'] for e in ev if e['event'] == 'planned']
    assert sum(e['event'] == 'finished' for e in ev) == 1, k   # one episode per file
    assert turns == sorted(turns) and turns[:1] == [1], k     # no interleaving
    ts = [e['t'] for e in ev if 't' in e]; spans[k] = (min(ts), max(ts))
ks = sorted(spans, key=lambda k: spans[k][0])
assert all(spans[b][0] >= spans[a][1] for a, b in zip(ks, ks[1:]))  # no overlap
print('ok')
PY
grep -rl 'sk-ant-' runs/ logs/ || echo 'no key in outputs'
```

Every line of `results.jsonl` is kept (a re-run instance appears twice;
the later line is scored); `summary.json` and `summary.md` count only the
latest line per instance.

## Troubleshooting

- Set `GRPC_VERBOSITY=ERROR` before gRPC loads (`aw_env.py` does this). Otherwise gRPC log lines end up in the adb output android_env parses, and app restores fail at random.
- On a failed adb call android_env runs `adb kill-server`. `aw_env.py` turns off only that restart, so other devices on the same adb server stay connected. `AW_ALLOW_ADB_RESTART=1` turns it back on.
- Atalanta's tree server must not turn off accessibility services, or AndroidWorld's success checks lose the screen tree. The adapter sets `ATALANTA_ANDROID_KEEP_A11Y=1`.
- android_env's accessibility forwarder sometimes crashes on connect and leaves a "keeps stopping" dialog. The adapter closes that one dialog before each task.
- If a task hangs in AndroidWorld's check or teardown, set `GRPC_ENABLE_FORK_SUPPORT=0`. The runner's per-task watchdog turns such a hang into an exception, and exceptions run again on resume.
