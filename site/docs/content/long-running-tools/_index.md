+++
title = "Long-Running Tools"
weight = 24
[extra]
group = "Guides"
+++

# Long-Running Tools

Some work takes longer than a tool call should: a test suite, a build, a
server, a subagent. A tool call must return before the agent can do anything
else, and the provider APIs require one result for every call the model makes.
A call that parks for minutes parks the whole session, and you cannot reach the
agent while it parks.

The pattern that works: return a small receipt at once, and deliver the outcome
later as an observation in the session mailbox.

```
model              host                        session
 |  call the tool    |
 |<-- receipt: id, log paths
 |                   | turn ends, session idle
 |               job exits
 |                   |-- observation + wake -->|
 |                   | next turn reads the observation
```

## Receipt and delivery

Start the job, hand back the id and the log paths, and let the `on_exit`
callback carry the outcome:

```lua
maki.api.register_tool({
  name = "watch",
  kind = "execute",
  permission = "run",
  schema = {
    type = "object",
    properties = {
      command = { type = "string", description = "The command to watch", required = true },
    },
  },
  handler = function(input, ctx)
    local session = ctx:session_id()
    local dir = maki.fs.joinpath(maki.env.logs_dir(), session, "watch")
    maki.fs.mkdir(dir, { parents = true })
    local ok, id = pcall(maki.fn.jobstart, input.command, {
      scope = { session = session },
      stdout = maki.fs.joinpath(dir, "stdout.log"),
      stderr = maki.fs.joinpath(dir, "stderr.log"),
      on_exit = function(job_id, code)
        maki.session.notify(
          string.format("[watch %d] %s exited with code %d", job_id, input.command, code),
          { session = session, wake = true }
        )
      end,
    })
    if not ok then
      return { llm_output = tostring(id), is_error = true }
    end
    return "watch "
      .. id
      .. " started. You will be notified when it exits. Keep working meanwhile."
  end,
})
```

`wake = true` starts a new turn when the session is idle, so the agent sees the
observation without the user doing anything. `ctx:session_id()` is captured in
the handler because the callback runs after the handler returned.

`jobstart` without `scope` defaults to `"task"`, which ends the job the moment
the handler returns — exactly when you need it to keep running. `scope = {
session = session }` ties the job to the session instead, so it survives past
this call.

## Why the receipt cannot arrive late

A provider request must carry a result for every tool call the model made, and
later turns build on that request. A result that arrives after newer turns
exist has no place to go in the API. The mailbox observation is how maki
represents an outcome that lands after its call ended: it joins the history and
the next turn reads it.

## Staying correct across reloads

A `/reload` drops the Lua callbacks of running jobs, the jobs keep running.
When the plugin loads again, find its jobs and re-arm each one:

```lua
for _, job in ipairs(maki.fn.joblist(nil) or {}) do
  if job.status == "running" then
    maki.fn.jobattach(job.id, { on_exit = my_on_exit(job) })
  end
end
```

`joblist(nil)` lists the jobs of your plugin across sessions, and each entry
carries its own `session`. Calling it at load time is fine. `maki.session.current()`
answers over the UI event loop instead, so a call made before the loop starts
draining (cold startup, not a `/reload`) returns `(nil, err)` rather than the
focused session.

This only finds jobs started with a session `scope`: `can_access` matches a
`"task"`-scoped job only while the same task call is still on the stack, so
`joblist` at load time never returns them, and `jobattach` has nothing to
re-arm.

Keep the outcome on disk when it matters (a meta file next to the logs). A job
that exits while the plugin was unloaded has no callback to deliver it, so the
next load compares disk state against `joblist` and reports what was missed.

## Waiting

Ending the turn and taking the wake is usually enough. When you truly need to
block, `jobwait` parks the caller until the job exits or the timeout passes:

- A timeout returns nil and leaves the job usable. You can wait again, and the
  `on_exit` callback still fires.
- While parked it delivers `on_stdout`, `on_stderr`, and `on_exit` as events
  arrive, like Neovim.
- An already-exited job answers from its snapshot without waiting.
- It parks the caller, so a slot chain cannot call it.

For bounded polling, `maki.async.sleep(ms)` suspends the coroutine on the async
executor without spinning, and other tasks keep running:

```lua
maki.async.run(function()
  for _ = 1, 40 do
    maki.async.sleep(250)
    if done_yet() then
      return
    end
  end
end)
```
