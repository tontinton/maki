local todos = {}
local popped = {}
local focused = nil
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

local function items_of(sid)
  return todos[sid or ""] or {}
end

local function is_focused(sid)
  return not focused or sid == focused
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

local function ensure_win(visible)
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
    visible = visible,
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

local function render_panel(items, visible)
  ensure_win(visible)
  buf:set_lines(build_lines(items))
  win:set_config({ height = #items + 2 })
  if win:is_visible() then
    maki.ui.set_status_hint(nil)
  else
    update_hint(items)
  end
end

local function hide_panel()
  if win and win:is_open() then
    win:hide()
  end
  maki.ui.set_status_hint(nil)
end

local function sync_panel(items, pop)
  if #items == 0 then
    hide_panel()
  else
    render_panel(items, pop)
  end
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use todo_write to plan and track multi-step tasks (must be 3+ steps). Update after EACH step, not only all at once.",
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

  restore = function(input)
    local items = input.todos or {}
    todos[focused or ""] = items
    if #items == 0 then
      return nil
    end
    render_panel(items, false)
    local body = maki.ui.buf()
    body:set_lines(build_lines(items))
    return body
  end,

  handler = function(input, ctx)
    local sid = ctx:session_id() or ""
    local items = input.todos or {}
    todos[sid] = items
    local pop = #items > 0 and not popped[sid]
    if pop then
      popped[sid] = true
    end
    if is_focused(sid) then
      sync_panel(items, pop)
    end
    return #items == 0 and "Todos cleared" or ""
  end,
})

local function toggle()
  local items = items_of(focused)
  if not win or #items == 0 then
    return
  end
  if win:is_visible() then
    win:hide()
    update_hint(items)
  elseif win:is_open() then
    win:show()
    maki.ui.set_status_hint(nil)
  else
    render_panel(items, true)
  end
end

maki.keymap.set("n", "<C-t>", toggle, { desc = "Toggle todo panel" })

maki.api.create_autocmd({ "TurnEnd", "SessionReset" }, {
  callback = function(ev)
    local sid = ev.data and ev.data.session_id or ""
    todos[sid], popped[sid] = nil, nil
    if is_focused(sid) then
      hide_panel()
    end
  end,
})

maki.api.create_autocmd("SessionFocusChanged", {
  callback = function(ev)
    focused = ev.data and ev.data.session_id
    -- Startup restore lands before the first focus event, so its items sit
    -- under the "" key; the first focused session is the one they belong to.
    if focused and todos[""] and not todos[focused] then
      todos[focused], todos[""] = todos[""], nil
    end
    sync_panel(items_of(focused), false)
  end,
})
