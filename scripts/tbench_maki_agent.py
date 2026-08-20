"""
Harbor agent wrapper for running maki on Terminal-Bench with analytics collection.

Requires: uv tool install harbor

Setup:
    harbor dataset download terminal-bench/terminal-bench-2

Run a single task:
    MOUNTS='["/usr/local/bin/maki:/mnt/maki:ro", "~/.maki/auth:/mnt/maki-auth:ro", "~/.maki/providers:/mnt/maki-providers:ro"]'

    harbor run \
      -t terminal-bench/fix-git \
      -m anthropic/claude-sonnet-4-6 \
      --agent-import-path tbench_maki_agent:MakiAgent \
      --mounts-json "$MOUNTS" \
      -n 1 -y

Run the full suite:
    harbor run \
      -d terminal-bench/terminal-bench-2 \
      -m anthropic/claude-sonnet-4-6 \
      --agent-import-path tbench_maki_agent:MakiAgent \
      --mounts-json "$MOUNTS" \
      -n 4 -y

Expand ~ in MOUNTS to your actual home directory if your shell does not
expand inside single quotes.

Analytics:
    After each run, analytics are appended to TBENCH_CSV (default: tbench_runs.csv)
    in the same format as collect.py, so analyze.py can read them directly:

        python scripts/analyze.py tbench_runs.csv

    Set TBENCH_CSV env var to override the output path.
"""

import json
import os
import shlex
from datetime import datetime, timezone
from pathlib import Path

from collect import append_csv, compute_cost, lookup_pricing
from harbor.agents.installed.base import (  # ty: ignore[unresolved-import]
    BaseInstalledAgent,
    with_prompt_template,
)
from harbor.environments.base import BaseEnvironment  # ty: ignore[unresolved-import]
from harbor.models.agent.context import AgentContext  # ty: ignore[unresolved-import]

AGENT_LOG_FILE = "maki.txt"
AGENT_LOG_PATH = f"/logs/agent/{AGENT_LOG_FILE}"


def parse_stream_json(log_text: str) -> tuple[dict, dict[int, dict], list[dict]]:
    """Parse maki --verbose --output-format stream-json output.

    Returns (result_summary, per_turn_usage, tool_calls) matching collect.py's format.
    """
    result = {}
    turn_usage: dict[int, dict] = {}
    tool_calls: list[dict] = []
    turn_index = 0
    last_msg_id = None
    model = ""
    session_id = ""

    for line in log_text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue

        msg_type = msg.get("type")

        if msg_type == "system":
            init = msg.get("init", msg)
            session_id = init.get("session_id", session_id)
            model = init.get("model", model)

        elif msg_type == "assistant":
            message = msg.get("message", {})
            usage = message.get("usage", {})
            content = message.get("content", [])
            msg_id = message.get("id")

            if msg_id and msg_id == last_msg_id:
                idx = turn_index - 1
            else:
                idx = turn_index
                turn_index += 1
            last_msg_id = msg_id

            turn_usage[idx] = usage
            for block in content:
                if block.get("type") == "tool_use":
                    tool_calls.append({
                        "turn": idx,
                        "name": block.get("name"),
                        "input": block.get("input", {}),
                    })

        elif msg_type == "result":
            session_id = msg.get("session_id", session_id)
            result = {
                "session_id": session_id,
                "model": model,
                "total_cost_usd": msg.get("total_cost_usd"),
                "duration_ms": msg.get("duration_ms"),
                "num_turns": msg.get("num_turns"),
                "usage": msg.get("usage", {}),
                "is_error": msg.get("is_error", False),
            }

    if not result.get("session_id"):
        result["session_id"] = session_id
    if not result.get("model"):
        result["model"] = model

    return result, turn_usage, tool_calls


class MakiAgent(BaseInstalledAgent):
    _last_instruction: str = ""

    @staticmethod
    def name() -> str:
        return "maki"

    def get_version_command(self) -> str | None:
        return "maki --version"

    async def install(self, environment: BaseEnvironment) -> None:
        await self.exec_as_root(
            environment,
            command="cp /mnt/maki /usr/local/bin/maki && chmod +x /usr/local/bin/maki && maki --version",
        )
        await self.exec_as_root(
            environment,
            command="if [ -d /mnt/maki-auth ]; then mkdir -p /root/.maki/auth && cp /mnt/maki-auth/* /root/.maki/auth/; fi",
        )
        await self.exec_as_root(
            environment,
            command="if [ -d /mnt/maki-providers ]; then mkdir -p /root/.maki/providers && cp /mnt/maki-providers/* /root/.maki/providers/ && chmod +x /root/.maki/providers/*; fi",
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name:
            raise ValueError("Model is required. Pass -m to harbor run.")

        self._last_instruction = instruction
        escaped = shlex.quote(instruction)
        await self.exec_as_agent(
            environment,
            command=(
                f"maki --print --yolo --verbose --output-format stream-json --model {self.model_name} "
                f"-- {escaped} 2>&1 </dev/null | tee {AGENT_LOG_PATH}"
            ),
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        log_path = self.logs_dir / AGENT_LOG_FILE
        if not log_path.exists():
            print(f"No maki log found at {log_path}")
            return

        log_text = log_path.read_text(encoding="utf-8", errors="replace")
        if not log_text.strip():
            print("Maki log is empty")
            return

        result, turn_usage, tool_calls = parse_stream_json(log_text)
        usage = result.get("usage", {})

        context.n_input_tokens = (
            usage.get("input_tokens", 0)
            + usage.get("cache_read_input_tokens", 0)
            + usage.get("cache_creation_input_tokens", 0)
        )
        context.n_cache_tokens = usage.get("cache_read_input_tokens", 0)
        context.n_output_tokens = usage.get("output_tokens", 0)

        cost = result.get("total_cost_usd") or 0
        if cost == 0:
            pricing = lookup_pricing(result.get("model", ""))
            cost = compute_cost(usage, pricing)
        context.cost_usd = cost

        context.metadata = {
            "session_id": result.get("session_id"),
            "model": result.get("model"),
            "duration_ms": result.get("duration_ms"),
            "num_turns": result.get("num_turns"),
            "is_error": result.get("is_error", False),
            "n_tool_calls": len(tool_calls),
        }

        csv_path = Path(os.environ.get("TBENCH_CSV", "tbench_runs.csv"))
        meta = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "agent": "maki",
            "session_id": result.get("session_id", ""),
            "tag": "tbench",
            "model": result.get("model", ""),
            "prompt": self._last_instruction[:200],
        }
        summary = {
            "total_cost_usd": cost,
            "duration_ms": result.get("duration_ms", 0),
            "num_turns": result.get("num_turns", 0),
            "usage": usage,
        }
        append_csv(csv_path, meta, summary, turn_usage, tool_calls)
        print(f"Analytics appended to {csv_path}")
