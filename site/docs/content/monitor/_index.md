+++
title = "Long-running commands"
weight = 32
[extra]
group = "Concepts"
+++

# Long-running commands

`bash` kills the command when its timeout hits (default 120s). For a test suite, a build, or a server that should keep running after the tool call returns, use [`monitor`](/docs/tools/#monitor).

```
monitor(command)     start it, get log paths back
                     keep working

exit observation     mailbox message with exit, paths, short tail
                     TUI starts a turn if the session is idle

monitor_wait         only when you have nothing else to do
monitor_peek         look now, do not wait
read stdout/stderr   post-mortem, not cat-through-bash
monitor_stop         kill the process group (safe after exit)
```

You will be notified when it exits, including on success. Do not `sleep` and do not poll.

## Logs

`monitor` is a plugin (`plugins/monitor/init.lua`) built on the same
session-owned jobs any plugin can use: `monitor(command)` starts a job that
survives a plugin reload and outlives the tool call, and streams its
stdout/stderr into a directory as they arrive:

```
~/.local/logs/maki/{session}/monitor-{id}/
  meta.json      command, pid, times, exit
  stdout.log     raw stdout
  stderr.log     raw stderr
```

`peek` and the exit observation keep a short tail in memory (the host tracks
this regardless of the plugin, so it survives a reload too); the files are
the source of truth for anything past that tail. On session end the host
kills the process group and the plugin removes that session's log directory.

The generated [tool reference](/docs/tools/#monitor) and [Lua API](/docs/lua-api/) list every parameter.
