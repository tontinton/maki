+++
title = "Token Economy"
weight = 30
[extra]
group = "Concepts"
+++

# Token Economy

Maki's whole design falls out of one fact about agent loops: the conversation is re-sent to the model on every turn.

```
turn 1   [system + prompt]                      ─► model ─► tool call
turn 2   [system + prompt + result 1]           ─► model ─► tool call
turn 3   [system + prompt + result 1 + 2]       ─► model ─► ...
```

A tool result does not cost its tokens once. It costs them again on every turn until the session ends or history is compacted. `cat` a 2000-line file on turn 2 of a 40-turn session and you pay for it 38 more times. Prompt caching softens the price, not the principle: cache reads still cost, and a bloated context also makes models measurably dumber.

So Maki attacks the two multipliers: how much each step adds to context, and how many steps there are.

## Smaller results

**index instead of read.** The `index` tool returns a tree-sitter skeleton of a source file: imports, types, signatures, line numbers. Usually 70-90% smaller than the file itself. The agent indexes first, then reads only the ranges it needs.

```
read main.rs                 index main.rs
────────────                 ─────────────────────────────
1400 lines in context        60 lines of signatures
                             + read offset=812 limit=40
```

**Subagents as garbage collectors.** A `task` subagent gets its own throwaway context. It can grep, read, and hit dead ends as much as it wants; only its final summary returns to your conversation. The mess is collected when it exits. Model tiers make this cheap too: delegate a search to a weak model at a fraction of the cost, keep the strong model for judgment.

```
main context                subagent context (discarded)
────────────                ────────────────────────────
task("find auth") ───────►  glob, grep ×6, read ×9, ...
                  ◄───────  "JWT middleware, auth.rs:120"
one line stays              ~20k tokens never seen
```

**Deferred MCP tools.** An MCP server with 100 tools would ship 100 definitions in every request. Maki loads a single `tool_search` tool instead; the model searches when it actually needs something and only the matches load. See [MCP](/docs/mcp/#tool-search).

**Truncation everywhere.** Tool output is capped (`agent.max_output_bytes`, `agent.max_output_lines`), overlong grep lines are skipped, and every builtin tool description nags the model to read only what it needs. The nagging works.

## Fewer round-trips

Every round-trip re-sends the context, so round-trips are the other half of the bill.

**batch** runs independent tool calls in one turn: one request, N results.

**code_execution** goes further: a Python sandbox where tools are async functions. Chained calls, loops, and filtering happen inside the sandbox; only what the script prints enters context.

```
without                          with code_execution
─────────────────────            ─────────────────────────────
glob        → 300 paths          results = gather(read × 300)
read × 300  → 300 files          filter in python
300 turns, every file            print("3 files call foo_v1")
in context forever               1 turn, 1 line in context
```

**Compaction** resets the multiplier when a session runs long: older turns are summarized and dropped. [Context](/docs/context/#when-the-window-fills) has the details.

## Watching it work

`/usage` shows the token breakdown of the current session, and `--output-format json` in [Headless Mode](/docs/headless/) reports `total_cost_usd` per run. Cheap is a feature you can measure.
