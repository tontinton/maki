local ToolView = require("maki.tool_view")

local MAIN_TASK = "main"
-- The panel already shows the live list, so the chat keeps each call folded to
-- its header until clicked.
local BODY_VIEW = { max_lines = 0, keep = "head" }

-- `sessions[session_id]`, one per session that wrote todos or took focus:
--   lists[task_id]  the latest list of each task. A subagent shares its
--                   session id with the parent, only the task id tells them apart
--   task            the task whose chat the session's tab shows
--   hidden          Ctrl+T hid the panel, until the turn ends
--   buf, win        the session's own panel, which lives in its tab
local sessions = {}
-- The session on screen, the one Ctrl+T acts on. It stays nil until the
-- first TaskFocusChanged.
local focused_session

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

local function state_of(sid)
  sessions[sid] = sessions[sid] or { lists = {}, task = MAIN_TASK }
  return sessions[sid]
end

local function shown_items(s)
  return s.lists[s.task] or {}
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

-- Shared by handler and restore, so a call looks the same live and reloaded,
-- and the UI's recorded clicks replay onto the same toggle.
local function list_body(items)
  return ToolView.restore_lines(build_lines(items), BODY_VIEW)
end

-- A plain buf made during a tool call becomes that call's chat body, and it
-- would mirror every later list from then on. A scratch buf stays ours.
local function open_panel(sid)
  local buf = maki.ui.buf({ scratch = true })
  local win = maki.ui.open_win(buf, {
    split = "panel",
    height = 4,
    order = 10,
    title = " Todos ",
    border = "rounded",
    focus = false,
    visible = false,
    session = sid,
    footer = {
      { "Ctrl+T", "to hide" },
    },
  })
  return buf, win
end

local function collapsed_hint(items)
  return {
    { string.format(" %d/%d ", count_done(items), #items), "foreground" },
    { "Ctrl+T", "keybind_key" },
    { " ", "" },
  }
end

-- The one place that draws a session. The panel shows the list, and while
-- Ctrl+T hides it the hint shows the count instead. Both live in the
-- session's tab, so a background session keeps them up to date and they are
-- right when you switch back. A hidden window still counts as open, so an
-- "open it unless it is" shortcut would keep the panel hidden for good.
local function sync(sid)
  local s = state_of(sid)
  local items = shown_items(s)
  local opts = { session = sid }
  if #items == 0 then
    if s.win then
      s.win:hide()
    end
    maki.ui.set_status_hint(nil, opts)
    return
  end
  if not (s.win and s.win:is_open()) then
    s.buf, s.win = open_panel(sid)
  end
  s.buf:set_lines(build_lines(items))
  s.win:set_config({ height = #items + 2 })
  if s.hidden then
    s.win:hide()
    maki.ui.set_status_hint(collapsed_hint(items), opts)
  else
    s.win:show()
    maki.ui.set_status_hint(nil, opts)
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
  permission = {},

  header = function(input)
    return string.format("%d todos", #(input.todos or {}))
  end,

  -- The panel is live-only, filled by handler calls during a turn. Replaying
  -- a transcript here would resurrect a finished list — and pop the panel —
  -- every time a session is reopened, so this only draws the call's list into
  -- the chat. A failed call renders the same, since the input is intact.
  restore = function(input)
    local items = input.todos or {}
    if #items == 0 then
      return nil
    end
    return list_body(items)
  end,

  -- Without a session there is no tab to put a panel in.
  handler = function(input, ctx)
    local items = input.todos or {}
    local sid, task = ctx:session_id(), ctx:task_id()
    if sid then
      local s = state_of(sid)
      s.lists[task] = items
      if task == s.task then
        sync(sid)
      end
    end
    if #items == 0 then
      return "Todos cleared"
    end
    return { llm_output = "", body = list_body(items) }
  end,
})

local function toggle()
  local s = sessions[focused_session]
  if not s or #shown_items(s) == 0 then
    return
  end
  s.hidden = not s.hidden
  sync(focused_session)
end

maki.keymap.set({ "n", "i" }, "<C-t>", toggle, { desc = "Toggle todo panel" })

-- Subagents run inside the parent's turn, so its end clears their lists too.
maki.api.create_autocmd("TurnEnd", {
  callback = function(ev)
    local sid = ev.data.session_id
    local s = sessions[sid]
    if s then
      s.lists, s.hidden = {}, nil
      sync(sid)
    end
  end,
})

-- The host already closes the panel and drops the hint when a session leaves
-- its tab, so forgetting the state is all that is left to do.
maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    sessions[ev.data.session_id] = nil
  end,
})

-- Also fires on a tab switch, which is how Ctrl+T learns the tab on screen.
-- Each tab has its own panel, so all we track per tab is whose chat it shows.
maki.api.create_autocmd("TaskFocusChanged", {
  callback = function(ev)
    focused_session = ev.data.session_id
    state_of(focused_session).task = ev.data.id
    sync(focused_session)
  end,
})
