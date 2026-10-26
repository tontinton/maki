-- Shared per-tool output limit options, so the tools that support them
-- cannot drift apart.

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local DEFAULT_MAX_LINE_BYTES = 500
local UTF8_CONTINUATION = "^[\128-\191]"

local M = {}

M.DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_LINE_BYTES
M.specs = {
  max_output_lines = { type = "integer", desc = "Override `agent.max_output_lines` for this tool." },
  max_output_bytes = { type = "integer", desc = "Override `agent.max_output_bytes` for this tool." },
}

function M.extend(spec)
  for name, s in pairs(M.specs) do
    spec[name] = s
  end
  return spec
end

--- Returns max_lines, max_bytes: tool override when set, agent-wide otherwise.
--- Both nil in full mode.
function M.resolve(opts, ctx)
  if ctx:output_mode() == "full" then
    return nil, nil
  end
  return opts.max_output_lines or ctx:config("max_output_lines", DEFAULT_MAX_OUTPUT_LINES),
    opts.max_output_bytes or ctx:config("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES)
end

--- opts.max_line_bytes, or 0 (no limit) in full mode.
function M.line_bytes(opts, ctx)
  return ctx:output_mode() == "full" and 0 or opts.max_line_bytes
end

local function split_lines(text)
  local lines, pos = {}, 1
  while true do
    local newline = text:find("\n", pos, true)
    if not newline then
      lines[#lines + 1] = text:sub(pos)
      return lines
    end
    lines[#lines + 1] = text:sub(pos, newline - 1)
    pos = newline + 1
  end
end

local function utf8_prefix(line, max_bytes)
  local cut = max_bytes
  while cut > 0 and line:find(UTF8_CONTINUATION, cut + 1) do
    cut = cut - 1
  end
  return line:sub(1, cut)
end

--- Cuts {text} to the limits keeping its head and its tail, since on a long
--- build or test run the errors and the summary sit at the end. Each end gets
--- half of every limit. A nil limit is unlimited. The marker between the two
--- notes how many lines and bytes it dropped.
function M.cut_middle(text, max_lines, max_bytes)
  if not max_lines and not max_bytes then
    return text
  end
  max_lines = max_lines or math.huge
  max_bytes = max_bytes or #text
  local lines = split_lines(text)
  if #lines <= max_lines and #text <= max_bytes then
    return text
  end

  local head_lines, head_bytes = math.ceil(max_lines / 2), math.ceil(max_bytes / 2)
  local head_end, bytes = 0, 0
  while head_end < head_lines and head_end < #lines and bytes + #lines[head_end + 1] + 1 <= head_bytes do
    head_end = head_end + 1
    bytes = bytes + #lines[head_end] + 1
  end
  if head_end == 0 then
    lines[1] = utf8_prefix(lines[1], head_bytes)
    head_end = 1
  end

  local tail_lines, tail_bytes = max_lines - head_lines, max_bytes - head_bytes
  local tail_start = #lines + 1
  bytes = 0
  while
    tail_start - 1 > head_end
    and #lines + 1 - tail_start < tail_lines
    and bytes + #lines[tail_start - 1] + 1 <= tail_bytes
  do
    tail_start = tail_start - 1
    bytes = bytes + #lines[tail_start] + 1
  end

  local head = table.concat(lines, "\n", 1, head_end)
  local tail = table.concat(lines, "\n", tail_start, #lines)
  local marker = "[truncated " .. (tail_start - 1 - head_end) .. " lines, " .. (#text - #head - #tail) .. " bytes]"
  if tail_start > #lines then
    return head .. "\n\n" .. marker
  end
  return head .. "\n\n" .. marker .. "\n\n" .. tail
end

return M
