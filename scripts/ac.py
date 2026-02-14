#!/usr/bin/env python3
"""Coding agent analytics collector - runs Claude Code or OpenCode headless, appends to CSV."""

import argparse
import csv
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


AGENTS = ("claude-code", "opencode")


def parse_args():
    p = argparse.ArgumentParser(description="Run coding agent with analytics collection")
    p.add_argument("prompt", help="Prompt to send")
    p.add_argument("--agent", choices=AGENTS, default="claude-code")
    p.add_argument("--model", default=None)
    p.add_argument("--max-turns", type=int, default=None)
    p.add_argument("--max-budget-usd", type=float, default=None)
    p.add_argument("--cwd", default=".")
    p.add_argument("--output", default="runs.csv", help="CSV output path")
    p.add_argument("--tag", default=None)
    return p.parse_args()


def build_cmd_claude(args):
    cmd = [
        "claude", "-p", "--verbose", "--output-format", "stream-json",
        "--dangerously-skip-permissions", args.prompt,
    ]
    if args.model:
        cmd += ["--model", args.model]
    if args.max_turns is not None:
        cmd += ["--max-turns", str(args.max_turns)]
    if args.max_budget_usd is not None:
        cmd += ["--max-budget-usd", str(args.max_budget_usd)]
    return cmd


def build_cmd_opencode(args):
    cmd = ["opencode", "run", "--format", "json", "--dir", args.cwd, args.prompt]
    if args.model:
        cmd += ["--model", args.model]
    return cmd


TOOL_DISPLAY_KEY = {
    "Read": "file_path", "Write": "file_path", "Edit": "file_path",
    "Glob": "pattern", "Grep": "pattern",
    "Bash": "command", "mcp_bash": "command",
}


def format_tool_summary(block):
    name = block.get("name", "?")
    key = TOOL_DISPLAY_KEY.get(name)
    if not key:
        return name
    arg = block.get("input", {}).get(key, "")
    return f"{name} {arg[:60]}"


def process_init(msg, meta):
    init = msg.get("init", msg)
    meta["session_id"] = init.get("session_id", meta["session_id"])
    meta["model"] = init.get("model", meta["model"])
    print(f"[init] session={meta['session_id'] or '?'} model={meta['model'] or '?'}", file=sys.stderr)


def process_assistant(msg, turn_index, turn_usage, all_tool_calls):
    message = msg.get("message", {})
    usage = message.get("usage", {})
    content = message.get("content", [])

    turn_usage[turn_index] = usage

    parts = []
    for b in content:
        btype = b.get("type")
        if btype == "tool_use":
            all_tool_calls.append({
                "turn": turn_index,
                "name": b.get("name"),
                "input": b.get("input", {}),
            })
            parts.append(f"tool_use {format_tool_summary(b)}")
        elif btype == "text":
            parts.append(f"text ({usage.get('output_tokens', '?')} tokens)")
    print(f"[turn {turn_index + 1}] assistant: {', '.join(parts) or 'empty'}", file=sys.stderr)


def process_result(msg, meta):
    if not meta.get("session_id"):
        meta["session_id"] = msg.get("session_id")

    cost = msg.get("total_cost_usd") or 0
    dur = (msg.get("duration_ms") or 0) / 1000
    print(f"[done] {msg.get('num_turns', 0)} turns, ${cost:.3f}, {dur:.1f}s", file=sys.stderr)

    return {
        "total_cost_usd": msg.get("total_cost_usd"),
        "duration_ms": msg.get("duration_ms"),
        "num_turns": msg.get("num_turns"),
        "usage": msg.get("usage", {}),
    }


def opencode_usage(tokens):
    return {
        "input_tokens": tokens.get("input", 0),
        "output_tokens": tokens.get("output", 0),
        "cache_read_input_tokens": tokens.get("cache", {}).get("read", 0),
        "cache_creation_input_tokens": tokens.get("cache", {}).get("write", 0),
    }


def process_opencode_stream(proc, meta):
    turn_usage = {}
    all_tool_calls = []
    turn_index = -1
    result_text = ""
    total_cost = 0
    total_tokens = {"input_tokens": 0, "output_tokens": 0,
                    "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    first_ts = None
    last_ts = None

    for raw_line in proc.stdout:
        line = raw_line.decode("utf-8", errors="replace").strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue

        msg_type = msg.get("type")
        part = msg.get("part", {})
        ts = msg.get("timestamp")
        if ts and first_ts is None:
            first_ts = ts

        if not meta.get("session_id"):
            meta["session_id"] = msg.get("sessionID")

        if msg_type == "step_start":
            turn_index += 1
            print(f"[turn {turn_index + 1}] start", file=sys.stderr)

        elif msg_type == "tool_use":
            state = part.get("state", {})
            inp = state.get("input", {})
            all_tool_calls.append({
                "turn": turn_index,
                "name": part.get("tool", ""),
                "input": inp,
            })
            print(f"[turn {turn_index + 1}] tool_use {part.get('tool', '?')}", file=sys.stderr)

        elif msg_type == "text":
            result_text = part.get("text", "")

        elif msg_type == "step_finish":
            last_ts = ts
            cost = part.get("cost", 0)
            total_cost += cost
            tokens = opencode_usage(part.get("tokens", {}))
            turn_usage[turn_index] = tokens
            for k in total_tokens:
                total_tokens[k] += tokens.get(k, 0)
            print(f"[turn {turn_index + 1}] finish reason={part.get('reason', '?')}", file=sys.stderr)

    duration_ms = (last_ts - first_ts) if (first_ts and last_ts) else 0
    num_turns = max(turn_index + 1, 0)
    print(f"[done] {num_turns} turns, ${total_cost:.3f}, {duration_ms / 1000:.1f}s", file=sys.stderr)

    summary = {
        "total_cost_usd": total_cost,
        "duration_ms": duration_ms,
        "num_turns": num_turns,
        "usage": total_tokens,
    }
    return summary, turn_usage, all_tool_calls, result_text


CSV_FIELDS = [
    "timestamp", "agent", "session_id", "tag", "model", "prompt",
    "run_cost_usd", "run_duration_ms", "run_num_turns",
    "run_input_tokens", "run_output_tokens", "run_cache_read", "run_cache_write",
    "turn", "tool_name", "tool_input",
    "turn_input_tokens", "turn_output_tokens", "turn_cache_read", "turn_cache_write",
]


def usage_fields(usage, prefix):
    return {
        f"{prefix}_input_tokens": usage.get("input_tokens", 0),
        f"{prefix}_output_tokens": usage.get("output_tokens", 0),
        f"{prefix}_cache_read": usage.get("cache_read_input_tokens", 0),
        f"{prefix}_cache_write": usage.get("cache_creation_input_tokens", 0),
    }


def append_csv(csv_path, meta, summary, turn_usage, tool_calls):
    run_base = {
        "timestamp": meta.get("timestamp", ""),
        "agent": meta.get("agent", ""),
        "session_id": meta.get("session_id", ""),
        "tag": meta.get("tag", ""),
        "model": meta.get("model", ""),
        "prompt": meta.get("prompt", ""),
        "run_cost_usd": summary.get("total_cost_usd", 0),
        "run_duration_ms": summary.get("duration_ms", 0),
        "run_num_turns": summary.get("num_turns", 0),
        **usage_fields(summary.get("usage", {}), "run"),
    }

    empty_turn = usage_fields({}, "turn")
    rows = []
    if tool_calls:
        for tc in tool_calls:
            turn_idx = tc.get("turn", 0)
            turn_fields = usage_fields(turn_usage.get(turn_idx, {}), "turn")
            rows.append({
                **run_base,
                "turn": turn_idx,
                "tool_name": tc.get("name", ""),
                "tool_input": json.dumps(tc.get("input", {}), separators=(",", ":")),
                **turn_fields,
            })
    else:
        rows.append({**run_base, "turn": 0, "tool_name": "", "tool_input": "", **empty_turn})

    write_header = not csv_path.exists()
    with open(csv_path, "a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=CSV_FIELDS)
        if write_header:
            w.writeheader()
        w.writerows(rows)


def process_claude_stream(proc, meta):
    turn_usage = {}
    all_tool_calls = []
    turn_index = 0
    summary = {}
    result_text = ""

    for raw_line in proc.stdout:
        line = raw_line.decode("utf-8", errors="replace").strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue

        msg_type = msg.get("type")
        if msg_type == "system":
            process_init(msg, meta)
        elif msg_type == "assistant":
            process_assistant(msg, turn_index, turn_usage, all_tool_calls)
            turn_index += 1
        elif msg_type == "result":
            result_text = msg.get("result", "")
            summary = process_result(msg, meta)

    return summary, turn_usage, all_tool_calls, result_text


STREAM_PROCESSORS = {
    "claude-code": (build_cmd_claude, process_claude_stream),
    "opencode": (build_cmd_opencode, process_opencode_stream),
}


def run(args):
    meta = {
        "prompt": args.prompt,
        "agent": args.agent,
        "model": args.model,
        "tag": args.tag,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "session_id": None,
    }

    build_cmd, process_stream = STREAM_PROCESSORS[args.agent]
    cmd = build_cmd(args)
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, cwd=args.cwd)
    assert proc.stdout is not None

    summary, turn_usage, all_tool_calls, result_text = process_stream(proc, meta)
    proc.wait()

    csv_path = Path(args.output)
    append_csv(csv_path, meta, summary, turn_usage, all_tool_calls)
    print(f"[csv] {csv_path}", file=sys.stderr)

    sys.stdout.write(result_text)
    return proc.returncode


if __name__ == "__main__":
    sys.exit(run(parse_args()))
