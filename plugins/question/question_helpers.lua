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

local function append_spans(out, src)
  for _, sp in ipairs(src) do
    out[#out + 1] = sp
  end
end

local function render_option_line(opt, selected)
  local line = {}
  if selected then
    line[1] = { "✓ ", "success" }
    line[2] = { opt.label, "success" }
  else
    line[1] = { "  ", "dim" }
    line[2] = { opt.label, "dim" }
  end
  if opt.description and opt.description ~= "" then
    line[#line + 1] = { " — ", "dim" }
    line[#line + 1] = { opt.description, selected and "success" or "dim" }
  end
  return line
end

function QuestionHelpers.render_card(questions, answers, opts)
  opts = opts or {}
  local width = opts.width or default_width()
  local dismissed = opts.dismissed or false
  local buf = maki.ui.buf()

  if dismissed then
    buf:line({ { "Dismissed by user", "dim" } })
    buf:line({})
  end

  for i, q in ipairs(questions) do
    q.options = q.options or {}

    local md_lines = question_md(q.question, width)
    for j, md_line in ipairs(md_lines) do
      local prefix = j == 1 and ("Q" .. i .. ". ") or "    "
      local line = { { prefix, "bold" } }
      append_spans(line, md_line)
      buf:line(line)
    end

    local ans = (not dismissed) and answers and answers[i] or nil
    if not ans or #ans == 0 then
      buf:line({ { "    (no answer)", "dim" } })
    else
      for _, opt in ipairs(q.options) do
        buf:line(render_option_line(opt, is_selected(ans, opt.label)))
      end
      local customs = find_custom(ans, q.options)
      if customs then
        for _, text in ipairs(customs) do
          buf:line({ { "    ✓ Custom: ", "success" }, { text, "success" } })
        end
      end
    end

    if i < #questions then
      buf:line({})
    end
  end

  return buf
end

return QuestionHelpers
