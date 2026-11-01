local L = require("lines")
local S = require("enums")
local todo_window = require("todo_window")

local MAIN_TASK = "main"

local DESCRIPTION = [[Create or update a structured todo list to track tasks.

**Use after EACH completed step!**

- Send the complete list each time (replace-all semantics).
- Use ONLY for multi-step work (3+ steps).
- Skip for trivial tasks.]]

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

local function shown_items(s)
  return s.lists[s.task] or {}
end

local opts = maki.api.register_options({
  panel_max_items = {
    default = 3,
    min = 1,
    desc = "How many todos the panel above the prompt shows; the rest stay behind /todo.",
  },
})

local function state_of(sid)
  sessions[sid] = sessions[sid] or { lists = {}, task = MAIN_TASK }
  return sessions[sid]
end

-- Shared by handler and restore, so a call looks the same live and reloaded.
local function list_body(items)
  local body = maki.ui.buf()
  body:set_lines(L.build_lines(items))
  return body
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
    { string.format(" %d/%d ", L.count_done(items), #items), "foreground" },
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
  local hint_opts = { session = sid }
  if #items == 0 then
    if s.win then
      s.win:hide()
    end
    maki.ui.set_status_hint(nil, hint_opts)
    return
  end
  if not (s.win and s.win:is_open()) then
    s.buf, s.win = open_panel(sid)
  end
  local rows = L.panel_lines(items, opts.panel_max_items)
  s.buf:set_lines(rows)
  s.win:set_config({ height = #rows + 2 })
  if s.hidden then
    s.win:hide()
    maki.ui.set_status_hint(collapsed_hint(items), hint_opts)
  else
    s.win:show()
    maki.ui.set_status_hint(nil, hint_opts)
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
              enum = S.statuses,
            },
            priority = {
              type = "string",
              enum = S.priorities,
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
    -- The /todo popup, if open, must track the list the call just wrote.
    todo_window.refresh()
    if #items == 0 then
      return "Todos cleared"
    end
    return { llm_output = "", body = list_body(items) }
  end,
})

maki.api.register_command({
  name = "/todo",
  description = "Show this session's full todo list",
  handler = function()
    -- The window lives in the focused tab, and that tab can switch between
    -- the main chat and a subagent's, so fetch re-reads the focus on every
    -- refresh rather than snapshotting it.
    todo_window.open(function()
      local s = focused_session and sessions[focused_session]
      local items = s and shown_items(s) or {}
      return items, L.count_done(items), #items
    end)
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
    -- A cleared focused list takes the /todo popup down with it.
    todo_window.refresh()
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
    -- Same session, different task: the popup's window survives but its list
    -- is the other task's now.
    todo_window.refresh()
  end,
})
