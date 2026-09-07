local MAIN_TASK = "main"

-- `todos[session_id][task_id]`: a subagent shares its session id with the
-- parent, only the task id tells them apart.
local todos = {}
-- Sessions where Ctrl+T hid the panel; forgotten when the turn ends.
local hidden = {}
-- Sessions that ran a turn since they were loaded. Restores of their
-- transcript are still in flight while the turn runs, so they are stale.
local live = {}
-- Nothing is focused until the first TaskFocusChanged; restores that land
-- before it still file under their own session, so they show up then.
local focused = { session = "", task = MAIN_TASK }
local buf, win

local STATUS_MARKERS = {
  completed = { "[✓]", "todo_completed" },
  in_progress = { "[•]", "todo_in_progress" },
  pending = { "[ ]", "todo_pending" },
  cancelled = { "[x]", "todo_cancelled" },
}

local DESCRIPTION = [[Create or update a structured todo list to track tasks.

**Use after EACH completed step!**

- Send the complete list each time (replace-all semantics).
- Use ONLY for multi-step work (3+ steps).
- Skip for trivial tasks.]]

local function items_of(sid, task)
  return todos[sid] and todos[sid][task] or {}
end

local function count_done(items)
  local n = 0
  for _, item in ipairs(items) do
    if item.status == "completed" then
      n = n + 1
    end
  end
  return n
end

local function update_hint(items)
  maki.ui.set_status_hint({
    { string.format(" %d/%d ", count_done(items), #items), "foreground" },
    { "Ctrl+T", "keybind_key" },
    { " ", "" },
  })
end

local function ensure_win()
  if buf and win and win:is_open() then
    return
  end
  buf = maki.ui.buf()
  win = maki.ui.open_win(buf, {
    split = "panel",
    height = 4,
    order = 10,
    title = " Todos ",
    border = "rounded",
    focus = false,
    visible = false,
    footer = {
      { "Ctrl+T", "to hide" },
    },
  })
end

local function build_lines(items)
  local lines = {}
  for _, item in ipairs(items) do
    local marker = STATUS_MARKERS[item.status] or STATUS_MARKERS.pending
    lines[#lines + 1] = {
      { marker[1] .. " " .. item.content, marker[2] },
    }
  end
  return lines
end

-- Rebuilt from the focused list on every change. A hidden window still
-- counts as open, so an "open it unless it is" shortcut would leave the
-- panel hidden for good.
local function sync_panel()
  local items = items_of(focused.session, focused.task)
  if #items == 0 then
    if win and win:is_open() then
      win:hide()
    end
    maki.ui.set_status_hint(nil)
    return
  end
  ensure_win()
  buf:set_lines(build_lines(items))
  win:set_config({ height = #items + 2 })
  if hidden[focused.session] then
    win:hide()
    update_hint(items)
  else
    win:show()
    maki.ui.set_status_hint(nil)
  end
end

local function store(sid, task, items)
  todos[sid] = todos[sid] or {}
  todos[sid][task] = items
  if sid == focused.session and task == focused.task then
    sync_panel()
  end
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use todo_write for multi-step tasks (3+ steps); update **after EACH step** (done + next in_progress), never batched at the end.",
})

maki.api.register_tool({
  name = "todo_write",
  description = DESCRIPTION,
  schema = {
    type = "object",
    required = { "todos" },
    properties = {
      todos = {
        type = "array",
        description = "The updated todo list",
        items = {
          type = "object",
          required = { "content", "status" },
          properties = {
            content = { type = "string", description = "Task description" },
            status = {
              type = "string",
              enum = { "pending", "in_progress", "completed", "cancelled" },
            },
            priority = {
              type = "string",
              enum = { "high", "medium", "low" },
            },
          },
        },
      },
    },
  },
  audiences = { "main", "research_sub", "general_sub" },

  header = function(input)
    return string.format("%d todos", #(input.todos or {}))
  end,

  -- A session load replays the transcript in order, so the last call wins
  -- and the panel picks up where the session left off. A rerender (click,
  -- theme change) replays one call that may be long superseded.
  restore = function(input, _output, _is_error, ctx)
    local items = input.todos or {}
    local sid = ctx:session_id() or ""
    if ctx:restore_reason() == "load" and not live[sid] then
      store(sid, ctx:task_id(), items)
    end
    if #items == 0 then
      return nil
    end
    local body = maki.ui.buf()
    body:set_lines(build_lines(items))
    return body
  end,

  handler = function(input, ctx)
    local items = input.todos or {}
    store(ctx:session_id() or "", ctx:task_id(), items)
    return #items == 0 and "Todos cleared" or ""
  end,
})

local function toggle()
  if #items_of(focused.session, focused.task) == 0 then
    return
  end
  hidden[focused.session] = not hidden[focused.session]
  sync_panel()
end

maki.keymap.set("n", "<C-t>", toggle, { desc = "Toggle todo panel" })

maki.api.create_autocmd("TurnStart", {
  callback = function(ev)
    live[ev.data.session_id] = true
  end,
})

-- Subagents run inside the parent's turn, so its end clears their lists too.
maki.api.create_autocmd({ "TurnEnd", "SessionReset", "SessionEnd" }, {
  callback = function(ev)
    local sid = ev.data and ev.data.session_id or ""
    todos[sid], hidden[sid] = nil, nil
    if ev.event ~= "TurnEnd" then
      live[sid] = nil
    end
    if sid == focused.session then
      sync_panel()
    end
  end,
})

-- Fires on a session switch too, so this is the one focus event the panel
-- needs to follow.
maki.api.create_autocmd("TaskFocusChanged", {
  callback = function(ev)
    focused = { session = ev.data.session_id, task = ev.data.id }
    sync_panel()
  end,
})
