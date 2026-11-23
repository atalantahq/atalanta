# Atalanta MCP tools: effects and retries

What each core tool of `atalanta mcp` does, changes and returns, and whether a call is safe to repeat. `--profile full` lists the tools these stand for (docs/MCP.md in the Atalanta repository).

| Tool | Changes the device | Returns | Safe to repeat |
|---|---|---|---|
| `observe` | no | `screen`: one line per element with refs (`mode: delta`: only changes); `screenshot`: a PNG saved to disk, its path and size (`inline: true` adds the image) | yes |
| `act` | yes, under the device's lease; waits up to 3 s for the screen to settle | what it did and what changed on screen. Extra keys: `clear_state` (launch), `until_visible` + `max_scrolls` (scroll), `timeout_ms` / `poll_interval_ms` (wait) | the failure says: `error.retry` is `safe`, `observe_first` (the action may have landed) or `no`; `error.delivered` says whether it reached the device |
| `run` | yes: `suite`/`yaml` run a suite on the device (a `yaml` you write runs too); `goal` runs an AI exploration that spends model calls | at once, a `task_id`; then poll `run {task_id}` | status polls: yes; starting: no (it starts another run) |
| `artifacts` | no | explanations and exports read from the last runs and this session | yes |
| `verify_change` | yes (replays or explores on the device) | a background task like `run`; its summary has the verdict and evidence folder | no |
