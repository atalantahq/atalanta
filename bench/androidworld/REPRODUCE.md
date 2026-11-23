# Reproducing Atalanta's AndroidWorld run

Everything here was run on an Apple Silicon Mac (macOS, 24 GB RAM). The
results and caveats are in [docs/BENCHMARKS.md](../../docs/BENCHMARKS.md#androidworld).

## 1. Harness

AndroidWorld at commit `55ddaf5edc6f6c25673aed343907d5b9dca66f84`
(google-research/android_world, 2026-10-23), Python 3.11 in its own venv:

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
5554/8554. AndroidWorld's own `run.py` hard-codes gRPC 8554; the scripts here
pass both ports to its `env_launcher` (`--console-port`, `--grpc-port`).

One-time app setup (AndroidWorld downloads its APKs from Google's bucket and
configures them, ~4 minutes):

```bash
cd bench/androidworld
../../ext/aw-venv/bin/python run.py run --perform-setup --out /tmp/aw-setup --script-dir plumbing --tasks ClockStopWatchRunning
```

## 3. Atalanta

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

AndroidWorld's protocol, as its `run.py` and `suite_utils` define it: all 116
tasks of the `android_world` family, `--n-task-combinations 1`, task seed 30,
step budget `int(10 * complexity)` per task, success = the agent said done
and the task's own device check passes.

```bash
export ANTHROPIC_API_KEY=...           # or the key of the provider you pass
$PY run.py run --out runs/full-sonnet-5-5 --model anthropic:claude-sonnet-5-5 --max-cost 150
```

`--max-cost` stops before starting a task once the running estimate (list
prices, cache reads counted at the full input price: an upper bound) passes
it. Interrupted or stopped: rerun the same command; finished instances are
skipped. `run.py summarize --out runs/full-sonnet-5-5` rewrites the summary.

Outputs in `--out`: `run.json` (model, provider, prices, Atalanta version,
harness SHA, seed, combinations, prompt settings), `results.jsonl` (one line
per instance: success, steps, wall time, tokens, cost, outcome, answer,
error), `summary.json` / `summary.md` (overall, per difficulty, per app),
and `episodes/<task>_<i>/` (Atalanta's events JSONL trajectory, its JSON
report, stderr).

Other providers: any `atalanta` preset works (`--model openai:…`,
`gemini:…`, …); pass `--price IN,OUT` for a cost estimate when the model
isn't in `run.py`'s price table.

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
