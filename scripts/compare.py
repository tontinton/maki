#!/usr/bin/env python3
"""Run collect.py across multiple agents with the same model, collect results into a single CSV."""

import argparse
import csv
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path


def _ts():
    return datetime.now().strftime("%H:%M:%S")

DEFAULT_AGENTS = ["claude-code", "maki", "opencode"]
DEFAULT_MODEL = "anthropic/claude-haiku-4-5"
COLLECT_SCRIPT = Path(__file__).parent / "collect.py"
ANALYZE_SCRIPT = Path(__file__).parent / "analyze.py"

BOLD = "\033[1m"
RESET = "\033[0m"
AGENT_COLORS = {
    "claude-code": "\033[38;5;172m",   # orange / light brown
    "maki":        "\033[35m",         # magenta
    "opencode":    "\033[34m",         # blue
}
DEFAULT_COLOR = "\033[37m"


def agent_color(agent):
    return AGENT_COLORS.get(agent, DEFAULT_COLOR)


def colored(agent, text):
    return f"{agent_color(agent)}{text}{RESET}"


def fmt_duration(seconds):
    m, s = divmod(int(seconds), 60)
    return f"{m}m{s:02d}s" if m else f"{s}s"


def parse_args():
    p = argparse.ArgumentParser(description="Compare coding agents head-to-head")
    p.add_argument("prompt", help="Prompt to send to each agent")
    p.add_argument("--agents", nargs="+", default=DEFAULT_AGENTS,
                   help=f"Agents to compare (default: {' '.join(DEFAULT_AGENTS)})")
    p.add_argument("--model", default=DEFAULT_MODEL,
                   help=f"Model for all agents (default: {DEFAULT_MODEL})")
    p.add_argument("--max-turns", type=int, default=None)
    p.add_argument("--max-budget-usd", type=float, default=None)
    p.add_argument("--cwd", default=".")
    p.add_argument("--output", default=None,
                   help="CSV output path (default: compare_<timestamp>.csv)")
    p.add_argument("--tag", default=None)
    return p.parse_args()


def resolve_model(agent, model):
    if agent == "opencode" and model.startswith("zai/"):
        return model.replace("zai/", "zai-coding-plan/", 1)
    if agent == "claude-code" and model.startswith("anthropic/"):
        return model.removeprefix("anthropic/")
    return model


def build_collect_cmd(args, agent, output):
    model = resolve_model(agent, args.model)
    cmd = [
        sys.executable, str(COLLECT_SCRIPT),
        args.prompt,
        "--agent", agent,
        "--model", model,
        "--output", str(output),
        "--cwd", args.cwd,
    ]
    if args.max_turns is not None:
        cmd += ["--max-turns", str(args.max_turns)]
    if args.max_budget_usd is not None:
        cmd += ["--max-budget-usd", str(args.max_budget_usd)]
    if args.tag:
        cmd += ["--tag", args.tag]
    return cmd


def merge_csvs(tmp_paths, output):
    writer = None
    with open(output, "w", newline="") as out:
        for p in tmp_paths:
            if not p.exists():
                continue
            with open(p, newline="") as f:
                reader = csv.reader(f)
                header = next(reader, None)
                if header is None:
                    continue
                if writer is None:
                    writer = csv.writer(out)
                    writer.writerow(header)
                for row in reader:
                    writer.writerow(row)


def main():
    args = parse_args()

    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    output = Path(args.output) if args.output else Path(f"compare_{ts}.csv")

    print(f"[{_ts()}] [compare] agents={args.agents} model={args.model}", file=sys.stderr)
    print(f"[{_ts()}] [compare] output={output}", file=sys.stderr)

    tmp_dir = tempfile.mkdtemp(prefix="compare_")
    procs = {}
    tmp_paths = {}

    start_times = {}
    for agent in args.agents:
        tmp_csv = Path(tmp_dir) / f"{agent}.csv"
        tmp_paths[agent] = tmp_csv
        cmd = build_collect_cmd(args, agent, tmp_csv)
        print(colored(agent, f"[{_ts()}] [{agent}] launching"), file=sys.stderr)
        start_times[agent] = time.monotonic()
        procs[agent] = subprocess.Popen(cmd)

    for agent, proc in procs.items():
        rc = proc.wait()
        elapsed = time.monotonic() - start_times[agent]
        status = "ok" if rc == 0 else f"exit {rc}"
        print(colored(agent,
            f"[{_ts()}] [{agent}] {BOLD}finished{RESET} {agent_color(agent)}({status}) in {fmt_duration(elapsed)}"
        ), file=sys.stderr)

    merge_csvs([tmp_paths[a] for a in args.agents], output)
    print(f"[{_ts()}] [compare] done -> {output}", file=sys.stderr)

    subprocess.call([sys.executable, str(ANALYZE_SCRIPT), str(output)])


if __name__ == "__main__":
    main()
