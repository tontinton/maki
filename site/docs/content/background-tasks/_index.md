+++
title = "Background Tasks"
weight = 24
[extra]
group = "Guides"
+++

# Background Tasks

A build, a test run, a dev server or a subagent can keep the agent waiting for minutes. Press `Ctrl+B` and the work keeps running while the agent carries on. The model gets a receipt at once, and the result arrives in the chat as a message when the work ends.

```
⠹ bash> Run the tests                                         ctrl+b background
    Compiling maki-agent v0.9.0
    Running unittests src/lib.rs

            press Ctrl+B

↗ bash> Run the tests (background · 12s · click to open)
```

Only tools that opt in can move. You move them with `Ctrl+B`, or the model asks for it. Calls that cannot move, such as `edit` or an MCP tool, finish before the model carries on.

## Move a call

`Ctrl+B` moves every running call that can move. It acts on the main chat from any tab, with two exceptions:

- In the tab of a running subagent with running calls, it moves that subagent's calls. See [Subagent calls](#subagent-calls).
- In a blocking subagent's tab without such calls, it moves the `task` call that owns the subagent.

While the agent is idle the key does nothing. Inside tmux, `Ctrl+B` is the prefix, so press it twice, or bind the `background` action to another key:

```lua
maki.keymap.set("n", "<C-g>", function()
    maki.ui.action("background")
end)
```

A call that can move shows a dim `ctrl+b background` hint at the right of its header, when the row has room. With several calls running, click one hint to move only that call.

The status bar confirms the key press:

| Flash | Meaning |
|-------|---------|
| `Moved 2 calls to the background` | Those calls moved. |
| `Nothing here can run in the background` | No running call can move. |
| `bash is waiting for your permission answer` | Answer the prompt first. |
| `batch is waiting on edit` | The tool declined and gave a reason. Here a batch child cannot move. |
| `16 background tasks already running` | The chat is at its [limit](#limits). |

### What can move

| Tool | Can move | Notes |
|------|----------|-------|
| `bash` | Yes | Once the process has started. See [Bash logs](#bash-logs). |
| `task` | Yes | The subagent moves with its transcript and still counts toward `max_concurrent`. One that waits for a free slot cannot move yet. |
| `batch` | When every running child can move | The receipt holds the children that already finished. The rest arrive with the final result. |
| `code_execution` | No | A budgeted script that calls other tools. |
| MCP tools | No | |
| `edit`, `write` and other fast tools | No | They finish before a move would matter. |

Plugin tools can opt in too, see [Make a plugin tool movable](#make-a-plugin-tool-movable).

### Subagent calls

When the tab in front is a running subagent, `Ctrl+B` and a click on the hint move that subagent's calls. Each gets its own task in `/tasks`, and its result goes back to the subagent.

If the subagent ends its turn while its moved calls still run, it waits, gets each result as a message and continues. Only then does it hand its final answer to the main chat. Stopping or closing the subagent stops its moved calls.

Only you can move a subagent's call. Inside a subagent, `background = true`, `ctx:background()` and `background_on_timeout` are ignored, because a subagent waits for its background work and a dev server would hang it.

## After a move

The receipt has two parts. The tool writes the first. Maki adds a sentence with the task id that tells the model to carry on or end its turn instead of polling, and to call `task_stop` if the task is no longer needed:

```
Running in the background. Last 3 lines:
...
Full output: <path>

Moved to the background by the user (task <id>). Its result arrives as a message. Do not poll or sleep for it: ...
```

A subagent has no `task_stop`. Its receipt tells it that the result arrives before it finishes.

Calls in the same round that did not move finish as usual.

While the task runs, its row shrinks to one line. Click it to open the task tab, where the output streams. When the task ends, the row says how:

```
↗ bash> Run the tests (background · 1m12s · click to open)
● bash> Run the tests (background · failed)
```

The ending is `done`, `failed`, `cancelled`, or `lost` for a task that died with maki, see [Lost tasks](#lost-tasks). A rewind, `/reload` and a session loaded from disk keep the ending.

While any task runs, the input box shows `1 task Ctrl+X` in its top border, so a dev server is hard to forget.

### Permissions

A moved subagent can still ask for permission. The prompt shows in the main view and names its task. Its writes follow the same file locks, stale-read checks and [plan mode](/docs/quick-start/#first-session) rules as any other subagent.

Other moved calls cannot prompt. A tool they call after the move is denied unless an [allow rule](/docs/permissions/) covers it. A prompt still open when the model moves the call, at a timeout for example, is denied.

### `/tasks`

`/tasks` or `Ctrl+X` lists moved calls and subagents, with their running time and, for subagents, their cost. A moved `batch` lists its calls one level in, and opening one shows that call alone:

```
  Running
  ⠹ 3 tools                                  30s · batch
    ⠹ Start the dev server                    30s · bash
    ✓ Review the diff                23s · $0.004 · task
```

`✓` marks a finished task, `✗` a failed one and `⊘` a stopped one.

| Key | Effect |
|-----|--------|
| `Enter` | Open the task. A bash task streams its output, a subagent shows its transcript. |
| `Ctrl+S` | Stop the task. A call inside a batch cannot stop alone, so the first press asks and the second stops the whole batch. A subagent inside a batch stops alone. |
| `Ctrl+D` | Delete a finished task from the list. |
| `Esc` | Close the picker. |

A moved bash or batch takes no messages, and its tab shows `esc esc stop`. A moved subagent still reads what you type in its tab. In a finished tab, `Esc` goes back to the main chat. So does typing or pasting, and the text goes to the main chat as if typed there.

## Stop a task

- Press `Ctrl+S` on its row in `/tasks`.
- Press `Esc` twice in the task tab.
- The model calls `task_stop` with the task id.

Stopping a bash task kills its process group and keeps the output so far. Who stopped it decides where that output goes:

- **You.** The main agent is not woken. It gets the output with your next message, and the transcript shows `Background bash cancelled: <summary>`. A subagent waiting on the call gets it right away.
- **`task_stop`.** The tool waits up to five seconds and returns the output. If the task needs longer, it returns `stop requested`, and the output arrives later as a message that does not start a turn.

`task_stop` only reaches tasks of its own chat. An unknown or finished id is an error that lists the running ids.

`Esc` in the main chat stops foreground work only. With tasks running, the flash says so: `Press esc again to stop... 1 task in background`.

## Results

A finished task reaches the model as a message that starts with `Background bash <id> (<summary>) finished:`, or `failed:`, followed by the output a foreground call would have returned. The transcript shows a short line with the call's description and a dot in the color of the outcome. Click the line to unfold the output:

```
● Background bash finished: Run the test suite
```

An idle chat wakes up when the message arrives. A working model gets it after the current round of tool results, so a build result can change its next step. This adds no request, because the next one was going out anyway.

## Let the model start tasks

The model can ask for a background run up front with `bash(background = true)` or `task(background = true)`. To keep every subagent in the foreground, turn the `task` input off:

```lua
maki.setup({
    plugins = {
        task = { allow_background = false },
    },
})
```

Both inputs take the same path as `Ctrl+B`, with the same `/tasks` row, result message and stop behavior. Maki's receipt sentence then starts with `Started in the background as asked`. A `task(background = true)` call that has to wait for a free subagent slot waits in the background, so the turn goes on.

Inside `batch`, a `background = true` call cannot move by itself. Once only such calls are still running, the batch moves as a whole, and the model gets the results that are already in.

If the move fails, for example with background tasks off, the call runs in the foreground and its result starts with `Could not start in the background (<reason>). Ran in the foreground.` A batch that cannot move adds that line once, at the top of its result.

### Move a slow command instead of killing it

Bash kills a command at its timeout. With `background_on_timeout`, it moves the command to the background instead:

```lua
maki.setup({
    plugins = {
        bash = { background_on_timeout = true },
    },
})
```

This also covers a `timeout` the model chose. The model treats it as how long it is willing to wait, and the receipt tells it that `task_stop` ends a command it no longer wants. Maki's receipt sentence then starts with `Hit its 120 s timeout and was moved to the background`. The tool description stays the same, so the prompt cache is unaffected.

## Bash logs

A bash command that moves writes its output to a log in the [state directory](/docs/configuration/#directory-layout):

```
<state>/tasks/<session>/<id>.log
```

The receipt names the log, so the model can read it when it needs the output before the result arrives, and reading a log of its own session never prompts. Logs of other sessions prompt like any path outside the project. Only a bounded tail stays in memory, so a dev server can run for days. When the tail dropped lines, the final result starts with `[N earlier lines are in the full output log]`.

At 32 MB the log moves to `<id>.log.1`, replacing the previous one, and a fresh `<id>.log` starts. Set the size with [`background_log_bytes`](/docs/configuration/#plugins-bash). Logs are deleted with their session.

A bash command inside a moved `batch` opens its log at its first output after the move, as `bash-<time>-<n>.log`, and the batch's final result names it.

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
| `ctx:set_parts(parts)` | List the pieces of the call under it in `/tasks`. See [Pieces of a call](#pieces-of-a-call). |
| `ctx:set_deadline(secs, { expire = "background" })` | Move the call at its deadline instead of ending it. |

A move the plugin starts itself, through `ctx:background()` or a deadline, needs the `agent` [plugin permission](/docs/lua-api/#plugin-permissions) and fails inside a subagent.

### Pieces of a call

A tool that runs other tools, like `batch`, can give each one its own row under the call in `/tasks`. Send the whole list every time something changes:

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

The rows show up once the call moves. A piece has no run of its own, so stopping it stops the whole call, and `maki.task.list()` marks it with `part = true`. Only the model's own call has pieces, so a nested call gets `nil, reason`.

## Lifecycle

| Event | Effect on background tasks |
|-------|----------------------------|
| End of the turn, `Esc` | None. Moved work is detached from the turn. |
| `/new` | Stopped. With tasks running, `/new` only flashes how many would stop, and `/new!` starts anyway. |
| Quit | Stopped. Quitting with running tasks takes a second press, after a flash such as `1 background task stops if you quit again`. |
| `/reload` | Stopped, because the reload replaces the Lua host they run in. The model learns that each result is lost. With tasks running, `/reload` only flashes how many would stop, and `/reload!` reloads anyway. `/trust` and package commands stop them without asking. |
| Rewind | Tasks whose receipt leaves the history are stopped, so no result answers a call the model no longer made. The picker confirms first: `Press enter again to rewind and stop 1 background task (cargo test)`. |
| Fork | Tasks stay with the original chat. The fork reports them as [lost](#lost-tasks). |
| Maki exits | Tasks die with their output. A resumed session reports them as lost. |

### Limits

A chat holds at most 16 background tasks. Moved tasks do not count against the limit on parallel tool calls. Moved subagents do count toward `max_concurrent`, and a foreground subagent waiting for a slot they hold shows `waiting for a free subagent slot`.

### Lost tasks

The session records which tasks the model still waits on. When you resume, fork or reload, the model gets a quiet message for each task that died:

```
Background bash <id> (<summary>) lost:

Maki exited or reloaded, or this session was forked, before it reported back.
```

The transcript names the task, as in `Background bash lost: Start the dev server`. The message does not start a turn and reaches the model with your next prompt. A fork only reports receipts that are still in its history.

## Headless, SDK and ACP

These modes have no `Ctrl+B`, but the model can still move work (`-p` only with `--background-timeout`) with `background = true`, `ctx:background()` and `background_on_timeout`:

| Mode | Results |
|------|---------|
| `-p` / `--print` | Off by default: every call blocks, and `background = true` runs in the foreground. With `--background-timeout`, the process stays alive after the run while background work runs, and each result starts a new turn. |
| SDK, stdin open | Results ride along with the next user message. No turn starts on its own. |
| SDK, stdin closed | The process stays alive as with `-p`, and each woken turn sends its own result. |
| ACP | Results arrive with the next prompt. No turn starts on its own. Closing the session stops the work. |

Nobody reads a moved subagent's prompts here, so they are denied and its tools need [allow rules](/docs/permissions/).

`--background-timeout <SECS>` bounds the wait, and `0` turns moves off. Unset, it is 600 in SDK mode and with `--exit-on-done`, and `-p` runs with moves off. `-p` promises one answer and a predictable exit, and a dev server the model moves never ends, so it would hold a CI job for the full timeout. In the TUI, `--exit-on-done` waits for background work under the same bound, except after a failed turn. Details are in [Headless Mode](/docs/headless/#background-tasks) and [ACP](/docs/acp/#background-tasks).
