local M = {}

local LF = "\n"
local CRLF = "\r\n"
local SNIPPET_MAX_CHARS = 32
local LINE_CUT_MARKER = "..."

-- A tie stays LF, so the answer never depends on which ending the file happens
-- to show first.
local function dominant_ending(content)
  local _, crlf = content:gsub(CRLF, "")
  local _, lf = content:gsub(LF, "")
  return crlf > lf - crlf and CRLF or LF
end

-- The `read` tool strips `\r`, so the model can only ever hand back LF text.
-- Applied straight to a CRLF file, an edit rewrites the lines it touches as LF
-- and leaves the rest alone, so the file ends up with both endings in it. Every
-- edit tool runs its transform on LF content here, and the file goes back out
-- with the ending it came with.
function M.preserve_line_endings(content, transform)
  local ending = dominant_ending(content)
  local result, err = transform((content:gsub(CRLF, LF)))
  if not result then
    return nil, err
  end
  if ending == LF then
    return result
  end
  return (result:gsub(CRLF, LF):gsub(LF, ending))
end

local function split_lines(s)
  if s:sub(-1) == "\n" then
    s = s:sub(1, -2)
  end
  return maki.split(s, "\n")
end

function M.snippet(text: string): string
  local line = text:match("[^\n]*") or ""
  local cut = utf8.offset(line, SNIPPET_MAX_CHARS + 1)
  if cut then
    return line:sub(1, cut - 1) .. "…"
  end
  return line
end

local function trim(s)
  return (s:match("^%s*(.-)%s*$"))
end

-- `read` cuts a long line and ends it with the marker, so that is all the model
-- can quote of it. A re-read shows the same cut line, hence the prefix match.
local function line_matches(line, wanted)
  local text = trim(line)
  if text == wanted then
    return true
  end
  if wanted:sub(-#LINE_CUT_MARKER) ~= LINE_CUT_MARKER then
    return false
  end
  local shown = wanted:sub(1, -#LINE_CUT_MARKER - 1)
  return text:sub(1, #shown) == shown
end

local function nearest_line(lines, wanted, from)
  for distance = 1, #lines do
    for _, i in ipairs({ from - distance, from + distance }) do
      if lines[i] and line_matches(lines[i], wanted) then
        return i
      end
    end
  end
  return nil
end

-- The model's line numbers go stale after its own earlier edit, and nothing
-- else can tell: the file is exactly as maki left it.
local function stale_line_error(lines, start_line, expected)
  local wanted = trim(expected)
  if line_matches(lines[start_line], wanted) then
    return nil
  end
  local found = wanted ~= "" and nearest_line(lines, wanted, start_line)
  local hint = found and string.format("that text is at line %d, shift the range", found) or "read the file again"
  return string.format(
    "line %d is %q, not %q: line numbers are stale, %s",
    start_line,
    M.snippet(lines[start_line]),
    M.snippet(expected),
    hint
  )
end

function M.replace_lines(content, start_line, end_line, new_string, expected_first_line)
  local trailing_nl = content:sub(-1) == "\n"
  local lines = split_lines(content)
  local inserting = end_line == nil

  if inserting then
    if start_line < 1 or start_line > #lines + 1 then
      return nil, string.format("start line %d out of range (1-%d)", start_line, #lines + 1)
    end
  else
    if start_line < 1 or start_line > #lines then
      return nil, string.format("start line %d out of range (1-%d)", start_line, #lines)
    end
    if end_line < start_line or end_line > #lines then
      return nil, string.format("end line %d out of range (%d-%d)", end_line, start_line, #lines)
    end
    local stale = expected_first_line and stale_line_error(lines, start_line, expected_first_line)
    if stale then
      return nil, stale
    end
  end

  local skip_from = inserting and start_line or start_line
  local skip_to = inserting and start_line - 1 or end_line

  local result = {}
  for i = 1, skip_from - 1 do
    result[#result + 1] = lines[i]
  end
  if new_string ~= "" or inserting then
    for _, line in ipairs(split_lines(new_string)) do
      result[#result + 1] = line
    end
  end
  for i = skip_to + 1, #lines do
    result[#result + 1] = lines[i]
  end

  local joined = table.concat(result, "\n")
  return trailing_nl and joined .. "\n" or joined
end

-- What the model needs to keep its line numbers right after an edit: every
-- line below the change moved by the same amount. Nothing when no line moved.
function M.shift_note(before, after)
  local old, new = split_lines(before), split_lines(after)
  local delta = #new - #old
  if delta == 0 then
    return nil
  end
  local shortest = math.min(#old, #new)
  local head = 0
  while head < shortest and old[head + 1] == new[head + 1] do
    head += 1
  end
  local tail = 0
  while tail < shortest - head and old[#old - tail] == new[#new - tail] do
    tail += 1
  end
  if tail == 0 then
    return nil
  end
  return string.format("lines after %d shifted by %+d", #old - tail, delta)
end

-- Insert `new_string` after line `after_line` and check arguments.
function M.insert_after(content, after_line, new_string)
  local lines = split_lines(content)
  if after_line < 0 or after_line > #lines then
    return nil, string.format("after line %d out of range (0-%d)", after_line, #lines)
  end
  return M.replace_lines(content, after_line + 1, nil, new_string)
end

return M
