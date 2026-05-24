local TextInput = require("maki.text_input")

local QuestionForm = {}

local MAX_HEIGHT_RATIO = 0.75
local CUSTOM_OPTION = "Type your own answer"
local CHROME = 3
local LABEL_INDENT = 4
local DESC_SEP = " — "
local DESC_SEP_WIDTH = 3
local ARROW_PREFIX = "    → "
local ARROW_PREFIX_W = 6
local SEPARATOR_CHAR = "─"

local MODE = {
  SELECTING = "selecting",
  EDITING_CUSTOM = "editing_custom",
  CONFIRMING = "confirming",
}

local NEWLINE_KEYS = {
  ["alt+enter"] = true,
  ["shift+enter"] = true,
  ["ctrl+enter"] = true,
  ["ctrl+j"] = true,
}

local function display_width(s)
  return utf8.len(s) or #s
end

local function split_at(s, max_cols)
  local total = utf8.len(s) or #s
  if total <= max_cols then
    return s, ""
  end
  local byte_end = utf8.offset(s, max_cols + 1)
  return s:sub(1, byte_end - 1), s:sub(byte_end)
end

local function wrap_spans(spans, max_width)
  if #spans == 0 then
    return { {} }
  end
  if max_width <= 0 then
    return { spans }
  end

  local lines, current, current_w = {}, {}, 0
  local pending_ws, pending_ws_w = "", 0

  local function flush()
    lines[#lines + 1] = current
    current, current_w = {}, 0
    pending_ws, pending_ws_w = "", 0
  end

  local function push(text, style, width)
    local last = current[#current]
    if last and last[2] == style then
      last[1] = last[1] .. text
    else
      current[#current + 1] = { text, style }
    end
    current_w = current_w + width
  end

  for _, span in ipairs(spans) do
    local style, remaining = span[2], span[1]
    while remaining ~= "" do
      local _, ws_end = remaining:find("^%s+")
      if ws_end then
        if current_w > 0 then
          local ws = remaining:sub(1, ws_end)
          pending_ws = pending_ws .. ws
          pending_ws_w = pending_ws_w + display_width(ws)
        end
        remaining = remaining:sub(ws_end + 1)
      else
        local word_end = remaining:find("%s") or (#remaining + 1)
        local word = remaining:sub(1, word_end - 1)
        remaining = remaining:sub(word_end)
        local word_w = display_width(word)
        if current_w + pending_ws_w + word_w <= max_width then
          if pending_ws_w > 0 then
            push(pending_ws, style, pending_ws_w)
            pending_ws, pending_ws_w = "", 0
          end
          push(word, style, word_w)
        elseif word_w <= max_width then
          flush()
          push(word, style, word_w)
        else
          while word ~= "" do
            if current_w >= max_width then
              flush()
            end
            local head, tail = split_at(word, max_width - current_w)
            push(head, style, display_width(head))
            word = tail
          end
        end
      end
    end
  end

  flush()
  return lines
end

local function append_wrapped(lines, spans, max_width, prefix, prefix_style, pad)
  for j, wrapped_line in ipairs(wrap_spans(spans, max_width)) do
    local row = { j == 1 and { prefix, prefix_style } or { pad, "" } }
    for _, sp in ipairs(wrapped_line) do
      row[#row + 1] = sp
    end
    lines[#lines + 1] = row
  end
end

local function has_confirm(state)
  if #state.questions > 1 then
    return true
  end
  return state.questions[1] and state.questions[1].multiple or false
end

local function initial_state(questions)
  return {
    mode = MODE.SELECTING,
    questions = questions,
    tab = 1,
    cursor = 1,
    answers = {},
    custom_input = TextInput.new(),
    rendered_questions = { width = nil, by_idx = {} },
  }
end

local function question_md(state, idx, width)
  local cache = state.rendered_questions
  if cache.width ~= width then
    cache.width = width
    cache.by_idx = {}
  end
  local hit = cache.by_idx[idx]
  if hit then
    return hit
  end
  local text = state.questions[idx].question
  local ok, lines = pcall(maki.ui.markdown, text, width)
  if not ok or type(lines) ~= "table" or #lines == 0 then
    lines = { { { text, "" } } }
  end
  cache.by_idx[idx] = lines
  return lines
end

local function is_selected(state, label)
  local ans = state.answers[state.tab]
  if not ans then
    return false
  end
  for _, v in ipairs(ans) do
    if v == label then
      return true
    end
  end
  return false
end

local function is_predefined(q, label)
  for _, opt in ipairs(q.options or {}) do
    if opt.label == label then
      return true
    end
  end
  return false
end

local function find_custom(state, q)
  local ans = state.answers[state.tab]
  if not ans then
    return nil, nil
  end
  for i, v in ipairs(ans) do
    if not is_predefined(q, v) then
      return i, v
    end
  end
  return nil, nil
end

local function toggle_option(state, label)
  local ans = state.answers[state.tab] or {}
  for i, v in ipairs(ans) do
    if v == label then
      table.remove(ans, i)
      state.answers[state.tab] = ans
      return
    end
  end
  ans[#ans + 1] = label
  state.answers[state.tab] = ans
end

local function goto_next_tab(state)
  if state.tab < #state.questions then
    state.tab = state.tab + 1
    state.cursor = 1
    state.mode = MODE.SELECTING
  else
    state.mode = MODE.CONFIRMING
  end
end

local function advance(state)
  if has_confirm(state) then
    goto_next_tab(state)
  else
    state.done = { type = "submit", answers = state.answers }
  end
end

local function handle_selecting(state, key)
  local q = state.questions[state.tab]
  local n = #q.options + 1

  if key == "up" then
    if state.cursor > 1 then
      state.cursor = state.cursor - 1
    end
  elseif key == "down" then
    if state.cursor < n then
      state.cursor = state.cursor + 1
    end
  elseif key == "enter" then
    if state.cursor == n then
      state.mode = MODE.EDITING_CUSTOM
      state.custom_input = TextInput.new()
      local _, existing = find_custom(state, q)
      if existing then
        state.custom_input:insert_text(existing)
      end
    elseif q.multiple then
      toggle_option(state, q.options[state.cursor].label)
    else
      state.answers[state.tab] = { q.options[state.cursor].label }
      advance(state)
    end
  elseif (key == "tab" or key == "right") and has_confirm(state) then
    goto_next_tab(state)
  elseif (key == "shift+tab" or key == "left") and has_confirm(state) then
    if state.tab > 1 then
      state.tab = state.tab - 1
      state.cursor = 1
    end
  elseif key == "esc" or key == "ctrl+c" then
    state.done = { type = "dismiss" }
  end
  return state
end

local function handle_editing_custom(state, key)
  if NEWLINE_KEYS[key] then
    state.custom_input:handle_key("newline")
  elseif key == "enter" then
    if state.custom_input:char_before_cursor() == "\\" then
      state.custom_input:handle_key("backspace")
      state.custom_input:handle_key("newline")
    else
      local text = state.custom_input:value()
      text = text:match("^%s*(.-)%s*$")
      local q = state.questions[state.tab]
      if text == "" then
        local ans = state.answers[state.tab]
        local idx = find_custom(state, q)
        if idx then
          table.remove(ans, idx)
          if #ans == 0 then
            state.answers[state.tab] = nil
          end
        end
        state.mode = MODE.SELECTING
      elseif q.multiple then
        local ans = state.answers[state.tab] or {}
        local idx = find_custom(state, q)
        if idx then
          ans[idx] = text
        else
          ans[#ans + 1] = text
        end
        state.answers[state.tab] = ans
        state.mode = MODE.SELECTING
      else
        state.answers[state.tab] = { text }
        state.mode = MODE.SELECTING
        advance(state)
      end
    end
  elseif key == "esc" then
    state.mode = MODE.SELECTING
  elseif key == "ctrl+c" then
    state.done = { type = "dismiss" }
  else
    state.custom_input:handle_key(key)
  end
  return state
end

local function handle_confirming(state, key)
  if key == "enter" then
    state.done = { type = "submit", answers = state.answers }
  elseif key == "shift+tab" or key == "left" then
    state.tab = #state.questions
    state.cursor = 1
    state.mode = MODE.SELECTING
  elseif key == "esc" or key == "ctrl+c" then
    state.done = { type = "dismiss" }
  end
  return state
end

local function handle_key(state, key)
  if state.mode == MODE.SELECTING then
    return handle_selecting(state, key)
  elseif state.mode == MODE.EDITING_CUSTOM then
    return handle_editing_custom(state, key)
  elseif state.mode == MODE.CONFIRMING then
    return handle_confirming(state, key)
  end
  return state
end

local function render_tab_bar(state)
  local spans = {}
  for i, q in ipairs(state.questions) do
    local label = q.header ~= "" and q.header or ("Q" .. i)
    local answered = state.answers[i] and #state.answers[i] > 0
    if i == state.tab and state.mode ~= MODE.CONFIRMING then
      spans[#spans + 1] = { " " .. label .. " ", "form_active" }
    elseif answered then
      spans[#spans + 1] = { " " .. label .. " ✓ ", "form_check" }
    else
      spans[#spans + 1] = { " " .. label .. " ", "form_inactive" }
    end
    spans[#spans + 1] = { "│", "form_separator" }
  end
  local confirm_label = " Review "
  if state.mode == MODE.CONFIRMING then
    spans[#spans + 1] = { confirm_label, "form_active" }
  else
    spans[#spans + 1] = { confirm_label, "form_inactive" }
  end
  return spans
end

local function separator_row(width)
  local n = math.max(0, width - 1)
  return { { string.rep(SEPARATOR_CHAR, n), "form_separator" } }
end

local function render_selecting(state, width)
  local lines = {}
  local focus_row = 1
  local reserved_top = 0
  local q = state.questions[state.tab]

  if has_confirm(state) then
    lines[#lines + 1] = render_tab_bar(state)
    lines[#lines + 1] = {}
    reserved_top = 2
  end

  for _, md_line in ipairs(question_md(state, state.tab, width - 1)) do
    append_wrapped(lines, md_line, width - 1, " ", "", " ")
  end
  lines[#lines + 1] = {}

  local opts = q.options or {}
  for i, opt in ipairs(opts) do
    local is_cur = (i == state.cursor)
    local checked = is_selected(state, opt.label)
    local pointer = is_cur and "▸ " or "  "
    local check = checked and "✓ " or "  "
    local spans = {}
    spans[#spans + 1] = { pointer, "form_arrow" }
    spans[#spans + 1] = { check, checked and "form_check" or "" }
    spans[#spans + 1] = { opt.label, is_cur and "form_active" or "" }
    if opt.description and opt.description ~= "" then
      local prefix_w = LABEL_INDENT + display_width(opt.label) + DESC_SEP_WIDTH
      local desc_lines = wrap_spans({ { opt.description, "form_description" } }, width - prefix_w)
      spans[#spans + 1] = { DESC_SEP, "form_description" }
      for _, sp in ipairs(desc_lines[1]) do
        spans[#spans + 1] = sp
      end
      lines[#lines + 1] = spans
      local pad = string.rep(" ", prefix_w)
      for j = 2, #desc_lines do
        local row = { { pad, "" } }
        for _, sp in ipairs(desc_lines[j]) do
          row[#row + 1] = sp
        end
        lines[#lines + 1] = row
      end
    else
      lines[#lines + 1] = spans
    end
    if is_cur then
      focus_row = state.cursor == 1 and 1 or #lines
    end

    if i < #opts then
      lines[#lines + 1] = separator_row(width)
    end
  end

  if #opts > 0 then
    lines[#lines + 1] = separator_row(width)
  end

  local custom_cur = (state.cursor == #opts + 1)
  local _, custom_text = find_custom(state, q)
  local custom_checked = custom_text ~= nil

  if state.mode == MODE.EDITING_CUSTOM then
    local input_lines = state.custom_input:render("  \xe2\x9d\xaf ", 4)
    local cursor_offset = state.custom_input:cursor_line()
    focus_row = #lines + cursor_offset
    for _, ln in ipairs(input_lines) do
      lines[#lines + 1] = ln
    end
  else
    local cptr = custom_cur and "▸ " or "  "
    local cchk = custom_checked and "✓ " or "  "
    local row = {
      { cptr, "form_arrow" },
      { cchk, custom_checked and "form_check" or "" },
      { CUSTOM_OPTION, custom_cur and "form_active" or "" },
    }
    if custom_checked then
      row[#row + 1] = { DESC_SEP .. custom_text, "form_description" }
    end
    lines[#lines + 1] = row

    if custom_cur then
      focus_row = #lines + 1
    end
  end

  lines[#lines + 1] = {}

  local footer
  if state.mode == MODE.EDITING_CUSTOM then
    footer = { { "Enter", "submit" }, { "Alt+Enter", "newline" }, { "Esc", "cancel" } }
  elseif q.multiple then
    footer = { { "Enter", "toggle" }, { "Tab", "next" }, { "Esc", "dismiss" } }
  else
    footer = { { "Enter", "submit" }, { "Tab", "next" }, { "Esc", "dismiss" } }
  end

  return { lines = lines, focus_row = focus_row, reserved_top = reserved_top, footer = footer }
end

local function render_confirming(state, width)
  local lines = {}
  lines[#lines + 1] = render_tab_bar(state)
  lines[#lines + 1] = {}
  lines[#lines + 1] = { { " Review your answers:", "bold" } }
  lines[#lines + 1] = {}

  local arrow_pad = string.rep(" ", ARROW_PREFIX_W)

  for i = 1, #state.questions do
    local ans = state.answers[i]
    local ans_text = (ans and #ans > 0) and table.concat(ans, ", ") or "(no answer)"
    local q_prefix = " " .. i .. ". "
    local q_prefix_w = display_width(q_prefix)
    local q_pad = string.rep(" ", q_prefix_w)

    for j, md_line in ipairs(question_md(state, i, width - q_prefix_w)) do
      append_wrapped(lines, md_line, width - q_prefix_w, j == 1 and q_prefix or q_pad, "", q_pad)
    end
    append_wrapped(
      lines,
      { { ans_text, "form_answer" } },
      width - ARROW_PREFIX_W,
      ARROW_PREFIX,
      "form_arrow",
      arrow_pad
    )

    if i < #state.questions then
      lines[#lines + 1] = separator_row(width)
    end
  end

  lines[#lines + 1] = {}
  local footer = { { "Enter", "submit" }, { "Shift+Tab", "back" }, { "Esc", "dismiss" } }
  return { lines = lines, focus_row = 1, reserved_top = 2, footer = footer }
end

local function render(state, width)
  if state.mode == MODE.CONFIRMING then
    return render_confirming(state, width)
  end
  return render_selecting(state, width)
end

QuestionForm._initial_state = initial_state
QuestionForm._handle_key = handle_key
QuestionForm._render = render
QuestionForm._is_selected = is_selected
QuestionForm._wrap_spans = wrap_spans
QuestionForm.MODE = MODE

function QuestionForm.open(questions)
  local state = initial_state(questions)
  local buf = maki.ui.buf()
  local max_h = math.floor(maki.ui.terminal_size().rows * MAX_HEIGHT_RATIO)

  local win = maki.ui.open_win(buf, {
    title = " Question ",
    height = max_h,
    width = "100%",
    border = "rounded",
    reserved_bottom = 1,
    focus = true,
    anchor = "SW",
    row = -1,
  })

  local width = win.width
  while true do
    local result = render(state, width)
    win:set_config({
      height = math.min(#result.lines + CHROME, max_h),
      reserved_top = result.reserved_top,
      footer = result.footer,
    })
    buf:set_lines(result.lines)
    win:set_cursor(result.focus_row)

    local ev = win:recv()
    if not ev or ev.type == "close" then
      return { type = "dismiss" }
    end

    if ev.type == "resize" then
      width = ev.width
      max_h = math.floor(maki.ui.terminal_size().rows * MAX_HEIGHT_RATIO)
    elseif ev.type == "paste" and state.mode == MODE.EDITING_CUSTOM then
      state.custom_input:insert_text(ev.text)
    elseif ev.type == "key" then
      state = handle_key(state, ev.key)
      if state.done then
        win:close()
        return state.done
      end
    end
  end
end

return QuestionForm
