#!/usr/bin/env python3
"""Analyze CSV output from collect.py - compare coding agents on cost, tools, and token efficiency."""

import argparse
import csv
import sys
from collections import defaultdict

from collect import compute_cost, lookup_pricing


def load_csv(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def unique_runs(rows):
    seen = {}
    for r in rows:
        sid = r["session_id"]
        if sid not in seen:
            seen[sid] = r
    return list(seen.values())


def fmt_num(n, decimals=0):
    if decimals:
        return f"{n:,.{decimals}f}"
    return f"{int(n):,}"


def fmt_pct(n):
    return f"{n:.1%}"


def fmt_table(title, headers, rows, aligns=None):
    if not rows:
        return
    aligns = aligns or ["<"] * len(headers)
    cols = [[h] + [r[i] for r in rows] for i, h in enumerate(headers)]
    widths = [max(len(str(c)) for c in col) for col in cols]

    def fmt_row(vals):
        return "  ".join(f"{v!s:{a}{w}}" for v, a, w in zip(vals, aligns, widths))

    print(f"\n{'=' * len(title)}")
    print(title)
    print(f"{'=' * len(title)}")
    print(fmt_row(headers))
    print("  ".join("-" * w for w in widths))
    for r in rows:
        print(fmt_row(r))
    print()


def safe_div(a, b):
    return a / b if b else 0


def total_input(r):
    """Total input tokens = uncached + cache_read + cache_write."""
    return (
        int(r.get("run_input_tokens") or 0)
        + int(r.get("run_cache_read") or 0)
        + int(r.get("run_cache_write") or 0)
    )


def run_cost(r):
    """Return cost, recomputing from tokens + pricing when reported cost is zero."""
    cost = float(r["run_cost_usd"] or 0)
    if cost > 0:
        return cost
    pricing = lookup_pricing(r.get("model", ""))
    if not pricing:
        return 0.0
    usage = {
        "input_tokens": int(r.get("run_input_tokens") or 0),
        "output_tokens": int(r.get("run_output_tokens") or 0),
        "cache_creation_input_tokens": int(r.get("run_cache_write") or 0),
        "cache_read_input_tokens": int(r.get("run_cache_read") or 0),
    }
    return compute_cost(usage, pricing)


def table_run_summary(rows):
    runs = unique_runs(rows)
    table_rows = []
    for r in runs:
        cost = run_cost(r)
        dur = float(r["run_duration_ms"] or 0) / 1000
        turns = int(r["run_num_turns"] or 0)
        inp = total_input(r)
        out = int(r["run_output_tokens"] or 0)
        prompt = r["prompt"][:50] + ("..." if len(r["prompt"]) > 50 else "")
        table_rows.append((
            r["agent"], r["model"], r["tag"] or "-", prompt,
            fmt_num(cost, 4), fmt_num(dur, 1), str(turns),
            fmt_num(inp), fmt_num(out),
        ))

    fmt_table(
        "Run Summary",
        ["Agent", "Model", "Tag", "Prompt", "Cost ($)", "Duration (s)", "Turns", "Total In Tok", "Output Tok"],
        table_rows,
        ["<", "<", "<", "<", ">", ">", ">", ">", ">"],
    )


def turn_total_input(r):
    """Total input tokens for a turn = uncached + cache_read + cache_write."""
    return (
        int(r.get("turn_input_tokens") or 0)
        + int(r.get("turn_cache_read") or 0)
        + int(r.get("turn_cache_write") or 0)
    )


def table_tool_usage(rows):
    agent_tools = defaultdict(lambda: defaultdict(lambda: {"count": 0, "inp": 0, "out": 0}))
    for r in rows:
        tool = r["tool_name"]
        if not tool:
            continue
        bucket = agent_tools[r["agent"]][tool]
        bucket["count"] += 1
        bucket["inp"] += turn_total_input(r)
        bucket["out"] += int(r["turn_output_tokens"] or 0)

    table_rows = []
    for agent in sorted(agent_tools):
        tools = agent_tools[agent]
        for tool in sorted(tools, key=lambda t: -tools[t]["count"]):
            b = tools[tool]
            table_rows.append((
                agent, tool, str(b["count"]),
                fmt_num(safe_div(b["inp"], b["count"])),
                fmt_num(safe_div(b["out"], b["count"])),
            ))
        total = {"count": 0, "inp": 0, "out": 0}
        for b in tools.values():
            for k in total:
                total[k] += b[k]
        table_rows.append((
            agent, "TOTAL", str(total["count"]),
            fmt_num(safe_div(total["inp"], total["count"])),
            fmt_num(safe_div(total["out"], total["count"])),
        ))
        table_rows.append(("", "", "", "", ""))

    fmt_table(
        "Tool Usage by Agent",
        ["Agent", "Tool", "Count", "Avg Total In Tok", "Avg Output Tok"],
        table_rows,
        ["<", "<", ">", ">", ">"],
    )


def table_token_efficiency(rows):
    agents = defaultdict(lambda: {
        "inp": 0, "out": 0, "cache_r": 0, "cache_w": 0,
        "cost": 0.0, "turns": 0, "runs": 0,
    })
    for r in unique_runs(rows):
        a = agents[r["agent"]]
        a["inp"] += int(r["run_input_tokens"] or 0)
        a["out"] += int(r["run_output_tokens"] or 0)
        a["cache_r"] += int(r["run_cache_read"] or 0)
        a["cache_w"] += int(r["run_cache_write"] or 0)
        a["cost"] += run_cost(r)
        a["turns"] += int(r["run_num_turns"] or 0)
        a["runs"] += 1

    table_rows = []
    for agent in sorted(agents):
        a = agents[agent]
        total_in = a["inp"] + a["cache_r"] + a["cache_w"]
        cache_hit = safe_div(a["cache_r"], total_in)
        out_in_ratio = safe_div(a["out"], total_in)
        cost_per_1k_out = safe_div(a["cost"], a["out"]) * 1000
        tok_per_turn = safe_div(a["out"], a["turns"])
        table_rows.append((
            agent, str(a["runs"]),
            fmt_num(total_in), fmt_num(a["out"]),
            fmt_num(a["cache_r"]), fmt_num(a["cache_w"]),
            fmt_pct(cache_hit), fmt_num(out_in_ratio, 3),
            fmt_num(cost_per_1k_out, 4), fmt_num(tok_per_turn),
        ))

    fmt_table(
        "Token Efficiency by Agent",
        ["Agent", "Runs", "Total In Tok", "Output Tok", "Cache Read", "Cache Write",
         "Cache Hit %", "Out/In Ratio", "$/1k Out Tok", "Out Tok/Turn"],
        table_rows,
        ["<", ">", ">", ">", ">", ">", ">", ">", ">", ">"],
    )


def main():
    p = argparse.ArgumentParser(description="Analyze ac.py CSV output")
    p.add_argument("csv", nargs="?", default="runs.csv", help="CSV file path")
    args = p.parse_args()

    try:
        rows = load_csv(args.csv)
    except FileNotFoundError:
        print(f"error: {args.csv} not found", file=sys.stderr)
        sys.exit(1)

    if not rows:
        print("error: CSV is empty", file=sys.stderr)
        sys.exit(1)

    table_run_summary(rows)
    table_tool_usage(rows)
    table_token_efficiency(rows)


if __name__ == "__main__":
    main()
