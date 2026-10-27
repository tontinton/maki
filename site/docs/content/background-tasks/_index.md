+++
title = "Background Tasks"
weight = 24
[extra]
group = "Guides"
+++

# Background Tasks

A slow build, a test run, a dev server or a subagent can keep the agent waiting for minutes. Press `Ctrl+B` and the work keeps running while the agent carries on. The model gets a receipt at once, and the result arrives in the chat as a message when the work ends.

```
⠹ cargo test --workspace                                      ctrl+b background
    Compiling maki-agent v0.9.0
    Running unittests src/lib.rs

            press Ctrl+B

↗ cargo test --workspace  (background · 12s · click or ctrl+x)
    Running in the background. Last 2 lines: ...
    Moved to the background by the user (task toolu_01...). Its result arrives as a message ...
```

A tool opts in to being moved, and then you or the model move it. Calls that cannot move, such as `edit` or an MCP tool, finish before the model carries on.

## Move a call

`Ctrl+B` moves every running call that can move, in the main chat. It works from any tab, because only the main chat can receive the result. While the agent is idle the key does nothing. You can rebind it, since it is the `background` action of `maki.ui.action`:

```lua
maki.keymap.set("n", "<C-g>", function()
    maki.ui.action("background")
end)
```

tmux uses `Ctrl+B` as its default prefix, so inside tmux you press it twice. The same rebinding works if you prefer to leave the prefix alone.

A call that can move right now shows a dim `ctrl+b background` hint at the right end of its header. The hint disappears when the row has no room for it, and it goes away when the call finishes. A bash command shows it once its process has started. With several calls running, click the hint of one row to move only that call and keep the others in the foreground.

A status bar flash confirms the key press:

| Flash | Meaning |
|-------|---------|
| `Moved 2 calls to the background` | Those calls moved. |
| `Nothing here can run in the background` | No running call can move. |
| `bash is waiting for your permission answer` | The call still waits on a permission prompt, so it stays. |
| `batch is waiting on edit` | A hook declined and gave a reason. Here a batch has a child that cannot move. |
| `16 background tasks already running` | The cap per chat. |

### What can move

| Tool | Can move | Notes |
|------|----------|-------|
| `bash` | Yes | Once the process has started. See [bash logs](#bash-logs). |
| `task` | Yes | A blocking subagent moves with its transcript. It still counts toward `max_concurrent`. One that waits for a free slot cannot move yet. |
| `batch` | Yes, when every running child can move | Finished children are reported inline in the receipt. The rest arrive with the final result. A child that cannot move, such as `edit`, makes the batch decline until it ends. |
| `code_execution` | No | A budgeted script that calls other tools. |
| MCP tools | No | |
| `edit`, `write` and other fast tools | No | They finish before a move would matter. |

## After a move

The model sees a receipt in two parts. The tool writes the first, and maki adds a sentence that names the task id and tells the model to continue with other work, end its turn, or call `task_stop` if the task is no longer needed. A bash receipt looks like this:

```
Running in the background. Last 3 lines:
...
Full output (read it to check progress): <path>

Moved to the background by the user (task <id>). Its result arrives as a message when it finishes. ...
```

Parallel calls that did not move finish as usual, then the model answers or keeps working.

The moved row in the transcript shows `↗` and `(background · 1m12s · click or ctrl+x)` while the task runs. It turns into `●` in the success, error or cancelled style when the task ends. Click the row to open the task. After a session restore the row shows the receipt text with a neutral `↗`.

While any task runs, the input box shows ` N tasks ctrl+x ` in its top border, so a dev server is hard to forget.

### `/tasks`

`/tasks` or `Ctrl+X` lists moved calls next to subagents, with a running time and, for subagents, their cost.

| Key | Effect |
|-----|--------|
| `Enter` | Open the task. A bash task shows its output still streaming, a subagent shows its transcript. |
| `Ctrl+S` | Stop the task. |
| `Ctrl+D` | Delete a finished task from the list. |
| `Esc` | Close the picker. |

A moved task takes no messages. The input box of its tab shows the placeholder `esc stop`.

## Stop a task

- Press `Ctrl+S` on its row in `/tasks`.
- Press `Esc` in the task tab.
- The model calls `task_stop` with the task id. It waits up to five seconds for the final output and returns it. If the task needs longer, the tool says the stop was requested and the output arrives later as a message.

Stopping a bash task kills its process group and keeps the output so far. A stop by you does not wake the model. The output reaches it with your next message, together with a `Background bash cancelled: <summary>` line in the transcript. A stop by `task_stop` posts no extra line, because the stop call already carries the output. `task_stop` only reaches tasks of its own chat. An unknown or finished id is an error that lists the running ids.

Pressing `Esc` in the main chat stops foreground work only. When tasks are running, the flash says so: `Press esc again to stop... 1 task in background`.

## Results

A finished task is pushed to the model as a message that starts with `Background bash <id> (<summary>) finished:` and holds the same output a foreground call would have returned. The transcript shows a shorter line:

```
● Background bash finished: cargo test --workspace
```

Click the line to expand the full result the model received. The task tab is gone after a delete or a restore, while this line stays in the history.

An idle chat wakes up when the message arrives. While the model is working, the message lands at its next step, after the current round of tool results, so a build result can change what it does next. It adds no extra request, because the request was going out anyway. When the task failed, `failed:` replaces `finished:` in that first line.

## Let the model start tasks

The model can ask for a task up front with `bash(background = true)`. The `task` tool offers the same `background` input once you turn it on:

```lua
maki.setup({
    plugins = {
        task = { allow_background = true },
    },
})
```

Both take the same path as `Ctrl+B`, so they get the same `/tasks` row, notice and stop behavior. The sentence maki adds to the receipt then starts with `Started in the background as asked`. A `task(background = true)` call that has to wait for a free subagent slot moves at once and keeps waiting in the background, so the model's turn does not wait with it.

If the move fails, for example because there is no live chat, the call runs in the foreground. Its result then starts with `Could not start in the background (<reason>). Ran in the foreground.`

A background subagent can ask for permissions. The prompt shows in the main view and names the task it belongs to. Earlier versions denied these requests. Moved subagents that write follow the same file locks, stale-read checks and [plan mode](/docs/quick-start/#first-session) block as any other subagent.

### Move a slow command instead of killing it

By default bash kills a command at its timeout. Set `background_on_timeout` to move it to the background instead:

```lua
maki.setup({
    plugins = {
        bash = { background_on_timeout = true },
    },
})
```

This applies to a timeout the model chose as well. The `timeout` param is how long the model is willing to wait, and the receipt tells it that `task_stop` ends a command it wanted dead. The sentence maki adds to the receipt then starts with `Hit its 120 s timeout and was moved to the background`. The tool description does not change, so prompt caching is unaffected. The bash options `background_on_timeout` and `background_log_bytes` are listed under [`plugins.bash`](/docs/configuration/#plugins-bash).

## Bash logs

A bash command that moves starts writing its output to a log file in the [state directory](/docs/configuration/#directory-layout):

```
<state>/tasks/<session>/<id>.log
```

The receipt carries the last lines and the log path, so the model reads the file to check progress. Reading a task log never prompts. Maki keeps only a bounded tail in memory, so a dev server can run for days. When the tail dropped lines, the final result starts with `[N earlier lines are in the full output log]`, and it names the log path.

Every time the log reaches 32 MB, the file moves to `<id>.log.1`, replacing the older one, and a fresh `<id>.log` starts. Only one old file is kept. Change the size with `background_log_bytes` in `plugins.bash`. Values above 1 GiB count as 1 GiB. The directory is deleted together with its session.

A bash command inside a moved `batch` has no log at the move, so the batch receipt carries no path for it. It opens its log with its first output after the move, and the batch's final result names the path. The file is called `bash-<time>-<n>.log` there, because the command has no task id of its own.

## Lifecycle

| Event | Effect on background tasks |
|-------|----------------------------|
| End of the turn, `Esc` | None. Moved work is detached from the turn. |
| `/new`, tab close, quit | Stopped, with no notice. Quitting with running tasks takes a second press, and a flash tells you how many tasks it will stop, such as `1 background task stops if you quit again`. |
| `/reload` | Moved calls keep running, like any call in flight. |
| Rewind | Tasks whose receipt leaves the history are stopped, so a result never answers a call the model no longer made. The picker confirms first: `This stops 1 background task (cargo test)`. |
| Fork | Tasks stay with the original chat. |
| Maki exits | Tasks die. Their output is lost, and a resumed session does not report them. |

A chat holds at most 16 background tasks. A foreground subagent that waits for a slot held by moved subagents shows `waiting for a free subagent slot`, so it does not look hung. Moved tasks do not count against the limit on parallel tool calls.

## Headless, SDK and ACP

These modes have no `Ctrl+B` and no live chat to deliver a result to. A command that hits its timeout is killed as usual, even with `background_on_timeout` set. A request to start a task in the background runs in the foreground, with the note about the failed move at the top of its result. The process ends when the run ends, so pending work would die with it anyway. See [Headless Mode](/docs/headless/) and [ACP](/docs/acp/).
