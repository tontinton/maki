local items = {}
local buf = nil
local win = nil
local seen_first = false

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

local function count_done()
  local n = 0
  for _, item in ipairs(items) do
    if item.status == "completed" then
      n = n + 1
    end
  end
  return n
end

local function update_hint()
  if #items == 0 then
    maki.ui.set_status_hint(nil)
    return
  end
  maki.ui.set_status_hint({
    { string.format(" %d/%d ", count_done(), #items), "foreground" },
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

local function render_panel(visible)
  ensure_win(visible)
  local lines = {}
  for _, item in ipairs(items) do
    local marker = STATUS_MARKERS[item.status] or STATUS_MARKERS.pending
    lines[#lines + 1] = {
      { marker[1] .. " " .. item.content, marker[2] },
    }
  end
  buf:set_lines(lines)
  win:set_config({ height = #items + 2 })
  if win:is_visible() then
    maki.ui.set_status_hint(nil)
  else
    update_hint()
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
    items = input.todos or {}
    if #items > 0 then
      render_panel(true)
    end
    return buf
  end,

  handler = function(input)
    items = input.todos or {}
    if #items == 0 then
      if win and win:is_open() then
        win:hide()
      end
      maki.ui.set_status_hint(nil)
      return "Todos cleared"
    end
    local first = not seen_first
    seen_first = true
    render_panel(first)
    return ""
  end,
})

local function toggle()
  if not win or #items == 0 then
    return
  end
  if win:is_visible() then
    win:hide()
    update_hint()
  elseif win:is_open() then
    win:show()
    maki.ui.set_status_hint(nil)
  else
    render_panel(true)
  end
end

maki.keymap.set("n", "<C-t>", toggle, { desc = "Toggle todo panel" })

maki.api.create_autocmd("TurnEnd", {
  callback = function()
    items = {}
    seen_first = false
    if win and win:is_open() then
      win:hide()
    end
    maki.ui.set_status_hint(nil)
  end,
})
