local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")
local color = require("maki.color")

local NO_MATCHES = "No files found"
local MAX_PER_CALL_LIMIT = 1000
local DIM_FACTOR = 0.3

local function has_context(groups)
  for _, group in ipairs(groups) do
    if #group.lines > 1 then
      return true
    end
  end
  return false
end

local function format_llm_output(entries)
  local parts = {}
  for i, entry in ipairs(entries) do
    if i > 1 then
      parts[#parts + 1] = ""
    end
    parts[#parts + 1] = entry.path .. ":"
    local ctx = has_context(entry.groups)
    for gi, group in ipairs(entry.groups) do
      if gi > 1 and ctx then
        parts[#parts + 1] = "  --"
      end
      for _, line in ipairs(group.lines) do
        local sep = line.is_match and ":" or " "
        parts[#parts + 1] = string.format("  %d%s %s", line.line_nr, sep, line.text)
      end
    end
  end
  return table.concat(parts, "\n")
end

local function count_matches(entries)
  local matches = 0
  for _, entry in ipairs(entries) do
    for _, group in ipairs(entry.groups) do
      for _, line in ipairs(group.lines) do
        if line.is_match then
          matches = matches + 1
        end
      end
    end
  end
  local f = #entries == 1 and "file" or "files"
  return string.format("%d matches in %d %s", matches, #entries, f)
end

local function grep_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 10, keep = "head" }
end

local function dim_spans(spans)
  local result = {}
  for _, span in ipairs(spans) do
    local style = span[2]
    if type(style) == "table" and style.fg then
      result[#result + 1] = { span[1], { fg = color.dim(style.fg, DIM_FACTOR) } }
    else
      result[#result + 1] = { span[1], "dim" }
    end
  end
  return result
end

local function build_grep_view(entries, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, grep_view_opts(ctx))

  local max_nr = 0
  for _, entry in ipairs(entries) do
    for _, group in ipairs(entry.groups) do
      for _, line in ipairs(group.lines) do
        if line.line_nr > max_nr then
          max_nr = line.line_nr
        end
      end
    end
  end
  local nr_fmt = "%" .. math.max(1, math.floor(math.log(max_nr + 1, 10)) + 1) .. "d "

  local hl_tasks = {}

  for _, entry in ipairs(entries) do
    if #entries > 1 then
      view:append({ { shorten_path(entry.path), "path" } })
    end

    local ctx_lines = has_context(entry.groups)
    local file_lines = {}

    for gi, group in ipairs(entry.groups) do
      if gi > 1 and ctx_lines then
        view:append({ { "  --", "dim" } })
      end
      for _, line in ipairs(group.lines) do
        view:append({ { string.format(nr_fmt, line.line_nr), "line_nr" }, { line.text } })
        file_lines[#file_lines + 1] = {
          idx = #view.all_lines,
          text = line.text,
          is_match = line.is_match,
        }
      end
    end

    hl_tasks[#hl_tasks + 1] = {
      ext = entry.path:match("%.([^%.]+)$") or "",
      lines = file_lines,
    }
  end

  view:finish()

  maki.async.run(function()
    for _, task in ipairs(hl_tasks) do
      local texts = {}
      for _, fl in ipairs(task.lines) do
        texts[#texts + 1] = fl.text
      end

      local highlighted = maki.ui.highlight(table.concat(texts, "\n"), task.ext, { independent = true })
      if highlighted then
        for i, fl in ipairs(task.lines) do
          local hl_spans = highlighted[i]
          if hl_spans then
            local nr_span = view.all_lines[fl.idx][1]
            local spans = fl.is_match and hl_spans or dim_spans(hl_spans)
            view:update_line(fl.idx, { nr_span, table.unpack(spans) })
          end
        end
      end
    end

    view:flush()
  end)

  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

local function parse_llm_output(text)
  local entries = {}
  local current
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    local path = line:match("^(%S.+):$")
    if path then
      current = { path = path, groups = { { lines = {} } } }
      entries[#entries + 1] = current
    elseif current then
      if line == "  --" then
        current.groups[#current.groups + 1] = { lines = {} }
      else
        local nr, sep, content = line:match("^%s+(%d+)([:]) (.*)$")
        if not nr then
          nr, sep, content = line:match("^%s+(%d+)( ) (.*)$")
        end
        if nr then
          local group = current.groups[#current.groups]
          group.lines[#group.lines + 1] = {
            line_nr = tonumber(nr),
            text = content or "",
            is_match = sep == ":",
          }
        end
      end
    end
  end
  return entries
end

maki.api.register_tool({
  name = "grep",
  kind = "search",
  description = [[Search file contents using regex.

- Respects .gitignore.
- Results grouped by file, sorted by modification time.
- Prefer speculative parallel searches over sequential rounds of glob+grep.
- Do NOT wrap the pattern in quotes. Do NOT double-escape (e.g. `\[` not `\\[`).
- Multi-line matching is auto-enabled when the pattern contains `\n`, `(?s)`, or `(?m)`.]],

  schema = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "Regex pattern", required = true },
      path = { type = "string", description = "Directory to search in (default: cwd)" },
      include = {
        type = "string",
        description = "File glob filter (e.g. *.c)",
        alias = "glob",
      },
      context_before = { type = "integer", description = "Context lines before match" },
      context_after = { type = "integer", description = "Context lines after match" },
      limit = { type = "integer", description = "Max match groups to return" },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    local pattern = (input.pattern or ""):gsub('"$', "")
    local spans = { { pattern, "tool" } }
    if input.include then
      spans[#spans + 1] = { " [" .. input.include .. "]", "dim" }
    end
    if input.path then
      spans[#spans + 1] = { " " .. shorten_path(input.path), "path" }
    end
    buf:line(spans)
    return buf
  end,

  restore = function(_input, output, _is_error, ctx)
    local entries = parse_llm_output(output)
    if #entries == 0 then
      return nil
    end
    return build_grep_view(entries, ctx)
  end,

  handler = function(input, ctx)
    local pattern = input.pattern
    if not pattern then
      return "error: pattern is required"
    end
    pattern = pattern:gsub('"$', "")

    local config = ctx:config()
    local search_limit = (config and config.search_result_limit) or 100
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)

    local limit = math.min(input.limit or search_limit, MAX_PER_CALL_LIMIT)

    local max_line_bytes = config and config.max_line_bytes

    local entries, err = maki.fs.grep(pattern, {
      path = input.path,
      include = input.include,
      context_before = input.context_before or 0,
      context_after = input.context_after or 0,
      limit = limit,
      max_line_bytes = max_line_bytes,
    })

    if not entries then
      return "error: " .. tostring(err)
    end

    if #entries == 0 then
      return { llm_output = NO_MATCHES }
    end

    for _, entry in ipairs(entries) do
      ctx:record_read(entry.path)
    end

    local llm_output = format_llm_output(entries)
    llm_output = truncate(llm_output, max_lines, max_bytes)

    return {
      llm_output = llm_output,
      body = build_grep_view(entries, ctx),
      annotation = count_matches(entries),
    }
  end,
})
