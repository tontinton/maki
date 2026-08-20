#!/usr/bin/env python3
"""Coding agent analytics collector - runs Maki, Claude Code, or OpenCode headless, appends to CSV."""

import argparse
import csv
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

AGENTS = ("maki", "claude-code", "opencode")

PER_MILLION = 1_000_000

# Pricing per million tokens (must match model.rs tiers).
PRICING = {
    "claude-3-haiku":    {"input": 0.25, "output": 1.25, "cache_write": 0.30, "cache_read": 0.03},
    "claude-3-5-haiku":  {"input": 0.80, "output": 4.00, "cache_write": 1.00, "cache_read": 0.08},
    "claude-haiku-4-5":  {"input": 0.80, "output": 4.00, "cache_write": 1.00, "cache_read": 0.08},
    "claude-3-sonnet":   {"input": 3.00, "output": 15.00, "cache_write": 0.30, "cache_read": 0.30},
    "claude-3-5-sonnet": {"input": 3.00, "output": 15.00, "cache_write": 3.75, "cache_read": 0.30},
    "claude-3-7-sonnet": {"input": 3.00, "output": 15.00, "cache_write": 3.75, "cache_read": 0.30},
    "claude-sonnet-4":   {"input": 3.00, "output": 15.00, "cache_write": 3.75, "cache_read": 0.30},
    "claude-sonnet-4-5": {"input": 3.00, "output": 15.00, "cache_write": 3.75, "cache_read": 0.30},
    "claude-opus-4-5":   {"input": 5.00, "output": 25.00, "cache_write": 6.25, "cache_read": 0.50},
    "claude-opus-4-6":   {"input": 5.00, "output": 25.00, "cache_write": 6.25, "cache_read": 0.50},
    "claude-3-opus":     {"input": 15.00, "output": 75.00, "cache_write": 18.75, "cache_read": 1.50},
    "claude-opus-4-0":   {"input": 15.00, "output": 75.00, "cache_write": 18.75, "cache_read": 1.50},
    "claude-opus-4-1":   {"input": 15.00, "output": 75.00, "cache_write": 18.75, "cache_read": 1.50},
}


def lookup_pricing(model_id):
    bare = model_id.split("/", 1)[1] if "/" in model_id else model_id
    for prefix, p in PRICING.items():
        if bare.startswith(prefix):
            return p
    return None


def compute_cost(usage, pricing):
    if not pricing:
        return 0.0
    inp = usage.get("input_tokens", 0)
    out = usage.get("output_tokens", 0)
    cw = usage.get("cache_creation_input_tokens", 0)
    cr = usage.get("cache_read_input_tokens", 0)
    return (
        inp * pricing["input"] / PER_MILLION
        + out * pricing["output"] / PER_MILLION
        + cw * pricing["cache_write"] / PER_MILLION
        + cr * pricing["cache_read"] / PER_MILLION
    )


RESET = "\033[0m"
AGENT_COLORS = {
    "claude-code": "\033[38;5;172m",
    "maki":        "\033[35m",
    "opencode":    "\033[34m",
}


def _color(agent):
    return AGENT_COLORS.get(agent, "")


_active_agent = ""


def _ts():
    return datetime.now(timezone.utc).astimezone().strftime("%H:%M:%S")


def _log(msg):
    c = _color(_active_agent)
    ts = _ts()
    prefix = f"[{ts}] [{_active_agent}]" if _active_agent else f"[{ts}]"
    print(f"{c}{prefix} {msg}{RESET}" if c else f"{prefix} {msg}", file=sys.stderr)


def parse_args():
    p = argparse.ArgumentParser(description="Run coding agent with analytics collection")
    p.add_argument("prompt", help="Prompt to send")
    p.add_argument("--agent", choices=AGENTS, default="maki")
    p.add_argument("--model", default=None)
    p.add_argument("--max-turns", type=int, default=None)
    p.add_argument("--max-budget-usd", type=float, default=None)
    p.add_argument("--cwd", default=".")
    p.add_argument("--output", default="runs.csv", help="CSV output path")
    p.add_argument("--tag", default=None)
    return p.parse_args()


def build_cmd_maki(args):
    cmd = [
        "maki", "-p", "--verbose", "--output-format", "stream-json",
        args.prompt,
    ]
    if args.model:
        cmd += ["-m", args.model]
    if args.max_turns is not None:
        cmd += ["--max-turns", str(args.max_turns)]
    return cmd


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

MAX_TOOL_PREVIEW_LINES = 5
MAX_TOOL_PREVIEW_LINE_LEN = 120


def format_tool_summary(block):
    name = block.get("name", "?")
    key = TOOL_DISPLAY_KEY.get(name)
    if not key:
        return name
    arg = block.get("input", {}).get(key, "")
    return f"{name} {arg[:60]}"


def format_tool_detail(block):
    name = block.get("name", "?")
    key = TOOL_DISPLAY_KEY.get(name)
    if not key:
        return None
    val = block.get("input", {}).get(key, "")
    if not val:
        return None
    lines = val.splitlines()
    preview = []
    for line in lines[:MAX_TOOL_PREVIEW_LINES]:
        if len(line) > MAX_TOOL_PREVIEW_LINE_LEN:
            preview.append(line[:MAX_TOOL_PREVIEW_LINE_LEN] + "...")
        else:
            preview.append(line)
    if len(lines) > MAX_TOOL_PREVIEW_LINES:
        preview.append(f"  ... ({len(lines) - MAX_TOOL_PREVIEW_LINES} more lines)")
    return "\n".join(f"  | {line}" for line in preview)


def process_init(msg, meta):
    init = msg.get("init", msg)
    meta["session_id"] = init.get("session_id", meta["session_id"])
    meta["model"] = init.get("model", meta["model"])
    _log(f"[init] session={meta['session_id'] or '?'} model={meta['model'] or '?'}")


def process_assistant(msg, turn_index, turn_usage, all_tool_calls):
    message = msg.get("message", {})
    usage = message.get("usage", {})
    content = message.get("content", [])

    turn_usage[turn_index] = usage

    parts = []
    details = []
    for b in content:
        btype = b.get("type")
        if btype == "tool_use":
            all_tool_calls.append({
                "turn": turn_index,
                "name": b.get("name"),
                "input": b.get("input", {}),
            })
            parts.append(f"tool_use {format_tool_summary(b)}")
            detail = format_tool_detail(b)
            if detail:
                details.append(detail)
        elif btype == "thinking":
            parts.append(f"thinking ({len(b.get('thinking', ''))} chars)")
        elif btype == "text":
            parts.append(f"text ({usage.get('output_tokens', '?')} tokens)")
    _log(f"[turn {turn_index + 1}] assistant: {', '.join(parts) or 'empty'}")
    for d in details:
        _log(d)


def process_result(msg, meta):
    if not meta.get("session_id"):
        meta["session_id"] = msg.get("session_id")

    cost = msg.get("total_cost_usd") or 0
    dur = (msg.get("duration_ms") or 0) / 1000
    _log(f"[done] {msg.get('num_turns', 0)} turns, ${cost:.3f}, {dur:.1f}s")

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
            _log(f"[turn {turn_index + 1}] start")

        elif msg_type == "tool_use":
            state = part.get("state", {})
            inp = state.get("input", {})
            all_tool_calls.append({
                "turn": turn_index,
                "name": part.get("tool", ""),
                "input": inp,
            })
            _log(f"[turn {turn_index + 1}] tool_use {part.get('tool', '?')}")

        elif msg_type == "text":
            result_text = part.get("text", "")

        elif msg_type == "step_finish":
            last_ts = ts
            tokens = opencode_usage(part.get("tokens", {}))
            turn_usage[turn_index] = tokens
            for k in total_tokens:
                total_tokens[k] += tokens.get(k, 0)
            _log(f"[turn {turn_index + 1}] finish reason={part.get('reason', '?')}")

    # opencode reports cost=0 for Anthropic models; compute from tokens + pricing
    pricing = lookup_pricing(meta.get("model", ""))
    total_cost = compute_cost(total_tokens, pricing)

    duration_ms = (last_ts - first_ts) if (first_ts and last_ts) else 0
    num_turns = max(turn_index + 1, 0)
    _log(f"[done] {num_turns} turns, ${total_cost:.3f}, {duration_ms / 1000:.1f}s")

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
        # Count tool calls per turn to split usage evenly (avoid double-counting).
        from collections import Counter
        calls_per_turn = Counter(tc.get("turn", 0) for tc in tool_calls)

        for tc in tool_calls:
            turn_idx = tc.get("turn", 0)
            raw = turn_usage.get(turn_idx, {})
            n = calls_per_turn[turn_idx]
            split = {k: v // n for k, v in raw.items() if isinstance(v, (int, float))} if n > 1 else raw
            turn_fields = usage_fields(split, "turn")
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
    last_msg_id = None

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
            msg_id = msg.get("message", {}).get("id")
            if msg_id and msg_id == last_msg_id:
                process_assistant(msg, turn_index - 1, turn_usage, all_tool_calls)
            else:
                process_assistant(msg, turn_index, turn_usage, all_tool_calls)
                turn_index += 1
            last_msg_id = msg_id
        elif msg_type == "result":
            result_text = msg.get("result", "")
            summary = process_result(msg, meta)

    return summary, turn_usage, all_tool_calls, result_text


STREAM_PROCESSORS = {
    "maki": (build_cmd_maki, process_claude_stream),
    "claude-code": (build_cmd_claude, process_claude_stream),
    "opencode": (build_cmd_opencode, process_opencode_stream),
}


def run(args):
    global _active_agent
    _active_agent = args.agent

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
    _log(f"[csv] {csv_path}")

    sys.stdout.write(result_text)
    return proc.returncode


if __name__ == "__main__":
    sys.exit(run(parse_args()))
