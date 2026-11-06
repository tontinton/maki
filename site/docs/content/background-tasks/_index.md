+++
title = "Background Tasks"
weight = 24
[extra]
group = "Guides"
+++

# Background Tasks

A build, a test run or a subagent can keep the agent waiting for minutes. Press `Ctrl+B` to move it to the background. The model gets a receipt at once and carries on, and the result arrives as a message when the work ends.

```
⠹ bash> Run the tests                                         ctrl+b background
    Compiling maki-agent v0.9.0
    Running unittests src/lib.rs

            press Ctrl+B

↗ bash> Run the tests (background · 12s · click to open)
```

## Move a call

`Ctrl+B` moves every running call that can move. It acts on the main chat from any tab, with two exceptions:

- In the tab of a running subagent with running calls, it moves that subagent's calls. See [Subagent calls](#subagent-calls).
- In a blocking subagent's tab without such calls, it moves the `task` call that owns the subagent.

A call that can move shows a dim `ctrl+b background` hint in its header. Click the hint to move only that call. When nothing moves, the status bar says why.

Inside tmux, `Ctrl+B` is the prefix, so press it twice, or bind the `background` action to another key:

```lua
maki.keymap.set("n", "<C-g>", function()
    maki.ui.action("background")
end)
```

### What can move

| Tool | Can move | Notes |
|------|----------|-------|
| `bash` | Yes | Once the process has started. See [Bash logs](#bash-logs). |
| `task` | Yes | Except while it waits for a free subagent slot. |
| `batch` | When every running child can move | The receipt holds the finished children. Each of the rest reports on its own as it finishes. |
| `code_execution` | No | A budgeted script that calls other tools. |
| MCP tools | No | |
| `edit`, `write` and other fast tools | No | They finish before a move would matter. |

Plugin tools can opt in, see [Make a plugin tool movable](#make-a-plugin-tool-movable).

### Subagent calls

A subagent's moved call gets its own task in `/tasks`, and its result goes back to the subagent. The subagent hands its final answer to the main chat only after every result has arrived. Stopping or closing the subagent stops its moved calls.

Only you can move a subagent's call. Inside a subagent, `background = true`, `ctx:background()` and `background_on_timeout` are ignored, because the subagent would wait for the work anyway and a dev server would hang it.

## After a move

The tool writes the receipt, and Maki adds a sentence with the task id. It tells the model to carry on or end its turn rather than poll, and to call `task_stop` if the task is no longer needed:

```
Running in the background. Last 3 lines:
...
Full output: <path>

Moved to the background by the user (task <id>). Its result arrives as a message. Do not poll or sleep for it: ...
```

The call's row shrinks to one line, except a batch row, which keeps every child's status live. Click it to open the task tab, where the output streams. When the task ends, the row says how: `done`, `failed`, `cancelled`, or `lost` for a task that died with Maki.

```
↗ bash> Run the tests (background · 1m12s · click to open)
● bash> Run the tests (background · failed)
```

While any task runs, the input box shows `1 task Ctrl+X` in its top border, and the session counts as working in the status bar and `/sessions`, even after the turn ended.

### Permissions

A moved subagent can still ask for permission. The prompt shows in the main view and names its task.

Other moved calls cannot prompt. A tool they call after the move is denied unless an [allow rule](/docs/permissions/) covers it. A prompt still open when the model moves the call, at a timeout for example, is denied.

### `/tasks`

`/tasks` or `Ctrl+X` lists moved calls and subagents, with their running time and, for subagents, their cost. A moved `batch` lists each of its calls as a task of its own:

```
  Running
  ⠹ Start the dev server                    30s · bash
  Finished
  ✓ Review the diff                23s · $0.004 · task
```

| Key | Effect |
|-----|--------|
| `Enter` | Open the task. |
| `Ctrl+S` | Stop the task. A call inside a batch stops the whole batch, so the first press asks. A subagent inside a batch stops alone. |
| `Ctrl+D` | Delete a finished task from the list. |

You can type to a moved subagent in its tab. Other task tabs have no input box, and what you type there goes to the main chat.

## Stop a task

- Press `Ctrl+S` on its row in `/tasks`.
- Press `Esc` twice in the task tab.
- The model calls `task_stop` with the task id.

`Esc` in the main chat leaves background tasks running. Stopping a bash task kills its process group and keeps the output so far. Who stopped it decides where that output goes:

- **You.** The main agent gets it with your next message, without being woken. A subagent waiting on the call gets it right away.
- **`task_stop`.** The tool waits up to five seconds and returns the output. A task that needs longer returns `stop requested`, and its output arrives later as a message that does not start a turn.

`task_stop` only reaches tasks of its own chat. An unknown or finished id is an error that lists the running ids.

## Results

A finished task reaches the model as a message that starts with `Background bash <id> (<summary>) finished:`, or `failed:`, followed by the output a foreground call would have returned. The transcript shows a one-line summary, and a click unfolds the output:

```
● Background bash finished: Run the test suite
```

An idle chat wakes up when the message arrives. A working model gets it after the current round of tool results, so a build result can change its next step without an extra request.

## Let the model start tasks

The model can ask for a background run up front with `bash(background = true)` or `task(background = true)`. Both behave like `Ctrl+B`, and the receipt sentence starts with `Started in the background as asked`. A subagent that has to wait for a free slot waits in the background.

To keep every subagent in the foreground:

```lua
maki.setup({
    plugins = {
        task = { allow_background = false },
    },
})
```

Inside `batch`, a `background = true` call cannot move by itself. Once only such calls are still running, the whole batch moves.

If the move fails, for example with background tasks off, the call runs in the foreground and its result starts with `Could not start in the background (<reason>). Ran in the foreground.`

### Move a slow command instead of killing it

Bash kills a command at its timeout. With `background_on_timeout`, it moves the command to the background instead, including at a `timeout` the model chose:

```lua
maki.setup({
    plugins = {
        bash = { background_on_timeout = true },
    },
})
```

The receipt sentence then starts with `Hit its 120 s timeout and was moved to the background`. The tool description does not change, so the prompt cache stays valid.

## Bash logs

A bash command that moves writes its output to a log in the [state directory](/docs/configuration/#directory-layout):

```
<state>/tasks/<session>/<id>.log
```

The receipt names the log, and the model reads logs of its own session without a prompt. Only a bounded tail stays in memory. When the tail dropped lines, the final result starts with `[N earlier lines are in the full output log]`.

At 32 MB the log moves to `<id>.log.1`, replacing the previous one, and a fresh `<id>.log` starts. Set the size with [`background_log_bytes`](/docs/configuration/#plugins-bash). Logs are deleted with their session.

A bash command inside a moved `batch` opens its log at its first output after the move, as `bash-<time>-<n>.log`, and its result names it.

## Make a plugin tool movable

A tool handler opts in with `ctx:on_background(fn)`. When the call moves, Maki runs `fn` and sends what it returns to the model as the receipt, a string or a result table. Returning `nil, reason` declines the move, and the reason shows in the flash. The handler keeps running, and what it returns later is the final result.

```lua
handler = function(input, ctx)
    ctx:on_background(function()
        return "Running in the background. Output: " .. log_path
    end)
    return run_slow_job(input) -- the final result, moved or not
end
```

`fn` must answer within 5 seconds. It can run more than once, and a move can still fail after it returns, so keep it idempotent. The receipt goes through [output hooks](/docs/hooks/) like any result.

| Method | Effect |
|--------|--------|
| `ctx:set_movable(bool)` | Hide the hint while `fn` would decline, and show it again. |
| `ctx:backgrounded()` | `true` once the call has moved, for example to switch to a bounded buffer. |
| `ctx:background()` | Move the call from the handler, as `bash(background = true)` does. Answers `true` or `nil, reason`. |
| `ctx:set_parts(parts)` | List the pieces of the call in `/tasks`. See [Pieces of a call](#pieces-of-a-call). |
| `ctx:report_part({ tool, name?, output, is_error?, format? })` | After the move, send one piece's result to the model now, worded as if `tool` had moved alone, with `name` as its summary. `format = "markdown"` makes the transcript render `output` as markdown, plain text otherwise. The final result is still due, so leave reported pieces out of it. Answers `true`, or `nil, reason` before the move or after the call ended. |
| `ctx:set_deadline(secs, { expire = "background" })` | Move the call at its deadline instead of ending it. |

A move the plugin starts itself, through `ctx:background()` or a deadline, needs the `agent` [plugin permission](/docs/lua-api/#plugin-permissions) and fails inside a subagent.

### Pieces of a call

A tool that runs other tools, like `batch`, can give each one its own row in `/tasks`, in place of the call's row. Send the whole list every time something changes:

```lua
ctx:set_parts({
    { id = "1", name = "Run the tests", tool = "bash", status = "working", body = buf },
    { id = "2", name = "Read README.md", tool = "read", status = "done" },
})
```

- `id` must stay the same between calls, or the piece opens a new row each time.
- `status` is `"working"`, `"done"`, `"error"` or `"cancelled"`.
- `body` is the buffer its tab shows.
- A piece left out of the list loses its row. One still `"working"` when the call ends ends with it.

The rows show up once the call moves. Stopping a piece stops the whole call. Only the model's own call has pieces, so a nested call gets `nil, reason`.

## Lifecycle

| Event | Effect on background tasks |
|-------|----------------------------|
| End of the turn, `Esc` | None. |
| `/new`, `/reload` | Stopped. With tasks running, both only flash how many would stop, and `/new!` or `/reload!` goes ahead. `/trust` and package commands reload and stop them without asking. |
| Quit | Stopped. With tasks running, quitting takes a second press. |
| Rewind | Tasks whose receipt leaves the history are stopped, after a confirmation. |
| Fork | Tasks stay with the original chat. The fork reports them as [lost](#lost-tasks). |
| Maki exits | Tasks die with their output. A resumed session reports them as lost. |

### Limits

A chat holds at most 16 background tasks. Moved tasks do not count against the limit on parallel tool calls. Moved subagents do count toward `max_concurrent`.

### Lost tasks

When you resume, fork or reload, the model learns of each task that died before reporting back:

```
Background bash <id> (<summary>) lost:

Maki exited or reloaded, or this session was forked, before it reported back.
```

The message does not start a turn and reaches the model with your next prompt. A fork only reports receipts that are still in its history.

## Headless, SDK and ACP

These modes have no `Ctrl+B`, but the model can still move work with `background = true`, `ctx:background()` and `background_on_timeout`:

| Mode | Results |
|------|---------|
| `-p` / `--print` | Off by default, so `background = true` runs in the foreground. With `--background-timeout`, the process stays alive while background work runs, and each result starts a new turn. |
| SDK, stdin open | Results ride along with the next user message. No turn starts on its own. |
| SDK, stdin closed | As `-p` with `--background-timeout`. |
| ACP | Results arrive with the next prompt. No turn starts on its own. Closing the session stops the work. |

Nobody answers a moved subagent's prompts here, so they are denied and its tools need [allow rules](/docs/permissions/).

`--background-timeout <SECS>` bounds the wait, and `0` turns moves off. It defaults to 600 in SDK mode and with `--exit-on-done`. `-p` keeps moves off by default because a dev server the model moves never ends and would hold a CI job for the full timeout. See [Headless Mode](/docs/headless/#background-tasks) and [ACP](/docs/acp/#background-tasks).
