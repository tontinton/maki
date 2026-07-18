local QuestionHelpers = {}

function QuestionHelpers.format_answer_list(questions, answers)
  local blocks = {}
  for i, q in ipairs(questions) do
    local lines = { "**Q" .. i .. ".** " .. q.question }
    lines[#lines + 1] = "**A" .. i .. ".**"
    local ans = answers[i]
    if ans and #ans > 0 then
      for _, v in ipairs(ans) do
        lines[#lines + 1] = "- " .. (v:gsub("\r?\n", "\n  "))
      end
    else
      lines[#lines + 1] = "- (no answer)"
    end
    blocks[#blocks + 1] = table.concat(lines, "\n")
  end
  return table.concat(blocks, "\n\n")
end

local function is_selected(answers, label)
  if not answers then
    return false
  end
  for _, v in ipairs(answers) do
    if v == label then
      return true
    end
  end
  return false
end

local function find_custom(answers, options)
  if not answers then
    return nil
  end
  local predefined = {}
  for _, opt in ipairs(options or {}) do
    predefined[opt.label] = true
  end
  local customs = {}
  for _, v in ipairs(answers) do
    if not predefined[v] then
      customs[#customs + 1] = v
    end
  end
  if #customs == 0 then
    return nil
  end
  return customs
end

local function default_width()
  local ok, size = pcall(maki.ui.terminal_size)
  if ok and type(size) == "table" and type(size.cols) == "number" then
    return math.max(40, size.cols - 8)
  end
  return 80
end

local function question_md(text, width)
  local ok, lines = pcall(maki.ui.markdown, text, width)
  if not ok or type(lines) ~= "table" or #lines == 0 then
    return { { { text, "" } } }
  end
  return lines
end

local function desc_md(text, width)
  local ok, lines = pcall(maki.ui.markdown, text, width)
  if not ok or type(lines) ~= "table" or #lines == 0 then
    return { { { text, "" } } }
  end
  return lines
end

local function append_spans(out, src)
  for _, sp in ipairs(src) do
    out[#out + 1] = sp
  end
end

local DESC_INDENT = "        "

function QuestionHelpers.render_card(questions, answers, opts)
  opts = opts or {}
  local width = opts.width or default_width()
  local dismissed = opts.dismissed or false
  local buf = maki.ui.buf()
  local expanded = {}
  local line_map = {}

  local function opt_key(q_idx, opt)
    return q_idx .. "\0" .. opt.label
  end

  local function is_expanded(q_idx, opt)
    return expanded[opt_key(q_idx, opt)] == true
  end

  local function toggle(q_idx, opt)
    local key = opt_key(q_idx, opt)
    expanded[key] = not expanded[key]
  end

  local function render()
    local lines = {}
    line_map = {}
    local line_no = 1

    if dismissed then
      lines[#lines + 1] = { { "Dismissed by user", "dim" } }
      lines[#lines + 1] = {}
      line_no = line_no + 2
    end

    for i, q in ipairs(questions) do
      q.options = q.options or {}

      local md_lines = question_md(q.question, width)
      for j, md_line in ipairs(md_lines) do
        local prefix = j == 1 and ("Q" .. i .. ". ") or "    "
        local line = { { prefix, "bold" } }
        append_spans(line, md_line)
        lines[#lines + 1] = line
        line_no = line_no + 1
      end

      local ans = (not dismissed) and answers and answers[i] or nil
      if not ans or #ans == 0 then
        lines[#lines + 1] = { { "    (no answer)", "dim" } }
        line_no = line_no + 1
      else
        for _, opt in ipairs(q.options) do
          local selected = is_selected(ans, opt.label)
          local line = {}
          if selected then
            line[1] = { "    ✓ ", "success" }
            line[2] = { opt.label, "success" }
          else
            line[1] = { "      ", "" }
            line[2] = { opt.label, "dim" }
          end
          local has_desc = opt.description and opt.description ~= ""
          if has_desc then
            local hint = is_expanded(i, opt) and " (−)" or " (+)"
            line[#line + 1] = { hint, "dim" }
          end
          lines[#lines + 1] = line
          line_map[line_no] = { q_idx = i, opt = opt }
          line_no = line_no + 1

          if has_desc and is_expanded(i, opt) then
            local desc_lines = desc_md(opt.description, width - #DESC_INDENT)
            for _, dl in ipairs(desc_lines) do
              local desc_line = { { DESC_INDENT, "" } }
              append_spans(desc_line, dl)
              lines[#lines + 1] = desc_line
              line_no = line_no + 1
            end
          end
        end

        local customs = find_custom(ans, q.options)
        if customs then
          for _, text in ipairs(customs) do
            lines[#lines + 1] = { { "    ✓ Custom: ", "success" }, { text, "success" } }
            line_no = line_no + 1
          end
        end
      end

      if i < #questions then
        lines[#lines + 1] = {}
        line_no = line_no + 1
      end
    end

    buf:set_lines(lines)
  end

  render()

  buf:on("click", function(ev)
    local info = line_map[ev.row]
    if info then
      toggle(info.q_idx, info.opt)
      render()
    end
  end)

  return buf
end

return QuestionHelpers
