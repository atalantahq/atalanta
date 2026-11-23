#!/usr/bin/env python3
"""An OpenAI-compatible endpoint backed by the Claude Code CLI (`claude -p`).

For machines with Claude Code logged in but no API key: Atalanta's
`openai-compatible` provider talks to this, and each request becomes one
`claude -p` call with no tools, no project settings and no MCP servers.
Structured output (`response_format: json_schema`) is passed through
`--json-schema`. Images (`image_url` data URIs) are passed as image blocks
through the CLI's stream-json input, so screenshots reach the model.

    python3 claude_bridge.py --port 8787 --log calls.jsonl
    ATALANTA_NL_PROVIDER=openai-compatible \
    ATALANTA_NL_API_URL=http://127.0.0.1:8787/v1 \
    ATALANTA_NL_MODEL=claude-sonnet-5-5 ATALANTA_NL_IMAGES=0 atalanta explore …

Every call is appended to --log as one JSON line: the model, the prompt
and reply text (the transcript the login-secrecy check reads), tokens,
the CLI's own cost figure, its raw usage object (cache reads and writes
apart) and the duration. Calls run at `--effort low` unless told otherwise.
"""

import argparse
import json
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
ARGS = None
# An empty working directory: no CLAUDE.md or project settings reach the model.
EMPTY = tempfile.mkdtemp(prefix="claude-bridge-")


def blocks_of(content):
    """OpenAI message content as Anthropic content blocks (text and images)."""
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    out = []
    for p in content or []:
        if not isinstance(p, dict):
            continue
        if p.get("type") == "text":
            out.append({"type": "text", "text": p.get("text", "")})
        elif p.get("type") == "image_url":
            url = (p.get("image_url") or {}).get("url", "")
            if url.startswith("data:") and ";base64," in url:
                media, data = url[5:].split(";base64,", 1)
                out.append({"type": "image",
                            "source": {"type": "base64", "media_type": media, "data": data}})
    return out


def text_of(content):
    if isinstance(content, str):
        return content
    parts = []
    for p in content or []:
        if isinstance(p, dict) and p.get("type") == "text":
            parts.append(p.get("text", ""))
    return "\n".join(parts)


def ask(body):
    model = body.get("model") or ARGS.model
    # --map NAME=MODEL: answer a client that asks for another vendor's model
    # (a judge that only attaches images for "gpt-*" names) with a Claude
    # model. The result is that Claude model's judgement, and is labelled so.
    requested = model
    model = dict(m.split("=", 1) for m in ARGS.map).get(model, model)
    system, blocks, n_images = "", [], 0
    for m in body.get("messages", []):
        if m.get("role") == "system":
            system += text_of(m.get("content")) + "\n"
        else:
            blocks.append({"type": "text", "text": f"[{m.get('role')}]"})
            for b in blocks_of(m.get("content")):
                n_images += b["type"] == "image"
                blocks.append(b)
    prompt = "\n\n".join(b["text"] for b in blocks if b["type"] == "text")
    cmd = [
        "claude", "-p", "--model", model, "--tools", "", "--setting-sources", "",
        "--strict-mcp-config", "--no-session-persistence",
        "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
    ]
    # The client's own reasoning_effort (Atalanta sends "low") unless the
    # bridge was started with --effort: hidden thinking was 79% of the paid
    # output tokens at the CLI's default (review 14 F2).
    effort = ARGS.effort or body.get("reasoning_effort")
    if effort:
        cmd += ["--effort", effort]
    if system.strip():
        cmd += ["--system-prompt", system]
    fmt = body.get("response_format") or {}
    schema = (fmt.get("json_schema") or {}).get("schema") if fmt.get("type") == "json_schema" else None
    if schema:
        cmd += ["--json-schema", json.dumps(schema)]
    elif fmt.get("type") == "json_object":
        blocks.append({"type": "text", "text": "Reply with one JSON object only."})
    msg = {"type": "user", "message": {"role": "user", "content": blocks}}
    started = time.time()
    out = subprocess.run(cmd, input=json.dumps(msg) + "\n", capture_output=True, text=True,
                         cwd=EMPTY, timeout=300)
    took = time.time() - started
    res = None
    for line in out.stdout.splitlines():
        try:
            j = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(j, dict) and j.get("type") == "result":
            res = j
    if res is None:
        raise RuntimeError(f"claude -p failed ({out.returncode}): {out.stderr[-500:] or out.stdout[-500:]}")
    if res.get("is_error"):
        raise RuntimeError(f"claude -p error: {res.get('result')}")
    if schema and res.get("structured_output") is not None:
        reply = json.dumps(res["structured_output"])
    else:
        reply = res.get("result", "")
    u = res.get("usage", {})
    prompt_tokens = (u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
                     + u.get("cache_read_input_tokens", 0))
    completion_tokens = u.get("output_tokens", 0)
    if ARGS.log:
        with LOCK, open(ARGS.log, "a") as f:
            f.write(json.dumps({
                "t": started, "model": model, "requested_model": requested, "seconds": round(took, 2),
                "cost_usd": res.get("total_cost_usd"), "prompt_tokens": prompt_tokens,
                "effort": effort or None, "usage": u,
                # Output tokens beyond the reply (~3.6 chars a token): the
                # model's hidden reasoning, which is billed but never seen.
                "hidden_tokens": max(0, completion_tokens - round(len(reply) / 3.6)),
                "images": n_images,
                "completion_tokens": completion_tokens, "system": system,
                "prompt": prompt, "reply": reply,
            }) + "\n")
    return {
        "id": res.get("session_id", "bridge"), "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": reply}}],
        "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
                  "total_tokens": prompt_tokens + completion_tokens,
                  "prompt_tokens_details": {"cached_tokens": u.get("cache_read_input_tokens", 0)}},
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        try:
            data, code = json.dumps(ask(body)).encode(), 200
        except Exception as e:  # noqa: BLE001 - reported to the client
            data, code = json.dumps({"error": {"message": str(e)}}).encode(), 502
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, default=8787)
    p.add_argument("--model", default="claude-sonnet-5-5")
    p.add_argument("--log")
    p.add_argument("--effort", default="low",
                   help="the CLI's --effort (low, medium, high, xhigh, max; '' for the CLI's "
                        "default). Atalanta's native Anthropic path plans at low effort; "
                        "without it most paid output tokens were hidden reasoning")
    p.add_argument("--map", action="append", default=[],
                   help="NAME=MODEL: serve requests for NAME with MODEL")
    ARGS = p.parse_args()
    for m in ARGS.map:
        # A client that asked for one model gets another: its own reports
        # will name the one it asked for. Say so where the operator can't
        # miss it (an iOSWorld judge was reported as gpt-5.4-mini while
        # claude-sonnet-5-5 answered, 2026-11-08).
        print(f"claude_bridge: requests for {m.split('=',1)[0]!r} are answered by {m.split('=',1)[1]!r}; "
              "label any score judged through this bridge with the model that answered", file=sys.stderr, flush=True)
    ThreadingHTTPServer(("127.0.0.1", ARGS.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
