local M = {}

M.NO_MATCH = "old_string not found in file"
M.MULTIPLE_MATCHES = "old_string matches multiple locations; add surrounding context to make it unique"
M.EMPTY_OLD_STRING = "old_string must not be empty"

local SINGLE_CANDIDATE_THRESHOLD = 0.0
local MULTI_CANDIDATE_THRESHOLD = 0.3
local CONTEXT_AWARE_LINE_MIN = 3
local CONTEXT_AWARE_MATCH_RATIO = 0.5
local INDENT_PATTERN = "^[ \t]*"

local function split_lines(s)
  local lines = {}
  local pos = 1
  while true do
    local nl = s:find("\n", pos, true)
    if not nl then
      lines[#lines + 1] = s:sub(pos)
      break
    end
    lines[#lines + 1] = s:sub(pos, nl - 1)
    pos = nl + 1
  end
  return lines
end

local function join_slice(lines, start, count)
  local block = {}
  for j = 0, count - 1 do
    block[#block + 1] = lines[start + j]
  end
  return table.concat(block, "\n")
end

local function table_slice(lines, start, count)
  local block = {}
  for j = 0, count - 1 do
    block[#block + 1] = lines[start + j]
  end
  return block
end

local function trim(s)
  return s:match("^%s*(.-)%s*$")
end

local function indent_of(line)
  return line:match(INDENT_PATTERN)
end

-- Only `old_string` was ever checked against the file, so the drift between the
-- two is the sole evidence of what the matcher forgave. Positions correspond
-- when the line counts agree, which every matcher but `block_anchor` guarantees;
-- otherwise only lines whose content agrees are the same line.
local function indent_drift(matched, find)
  local file_lines, model_lines = split_lines(matched), split_lines(find)
  local paired_by_position = #file_lines == #model_lines
  local drift, forgave = {}, false
  for i = 1, math.min(#file_lines, #model_lines) do
    local file_line, model_line = file_lines[i], model_lines[i]
    local same_line = paired_by_position or trim(file_line) == trim(model_line)
    if same_line and trim(file_line) ~= "" and trim(model_line) ~= "" then
      local model_indent, file_indent = indent_of(model_line), indent_of(file_line)
      if drift[model_indent] == nil then
        drift[model_indent] = file_indent
        forgave = forgave or model_indent ~= file_indent
      end
    end
  end
  return forgave and drift or nil
end

-- A column the model never used is extrapolated from the deepest one it did, so
-- a level the replacement adds keeps the shape the model gave it. A column
-- shallower than any in the block belongs to a line that leaves the block, a new
-- top level definition say, so it stays where the model put it.
local function rebase(drift, indent)
  local exact_column = drift[indent]
  if exact_column then
    return exact_column
  end
  local deepest
  for model_indent in pairs(drift) do
    if #model_indent < #indent and indent:sub(1, #model_indent) == model_indent then
      if not deepest or #model_indent > #deepest then
        deepest = model_indent
      end
    end
  end
  return deepest and drift[deepest] .. indent:sub(#deepest + 1) or indent
end

-- The model can write `old_string` sloppily and `new_string` in the file's own
-- frame, and then there is nothing left to correct.
local function written_in_file_frame(drift, replacement)
  local file_columns = {}
  for _, file_indent in pairs(drift) do
    file_columns[file_indent] = true
  end
  for _, line in ipairs(split_lines(replacement)) do
    if trim(line) ~= "" and not file_columns[indent_of(line)] then
      return false
    end
  end
  return true
end

-- Corrects exactly what the match forgave in `old_string`, and nothing else.
local function reindent(matched, find, replacement)
  local drift = indent_drift(matched, find)
  if not drift or written_in_file_frame(drift, replacement) then
    return replacement
  end

  local out = {}
  for i, line in ipairs(split_lines(replacement)) do
    local indent = indent_of(line)
    if trim(line) == "" then
      out[i] = line
    else
      out[i] = rebase(drift, indent) .. line:sub(#indent + 1)
    end
  end
  return table.concat(out, "\n")
end

local function escape_pattern(s)
  return (s:gsub("[%(%)%.%%%+%-%*%?%[%]%^%$]", "%%%0"))
end

local ESCAPE_MAP = {
  n = "\n",
  t = "\t",
  r = "\r",
  ["'"] = "'",
  ['"'] = '"',
  ["`"] = "`",
  ["\\"] = "\\",
  ["$"] = "$",
  ["\n"] = "\n",
}

local function unescape(s)
  local result = {}
  local i = 1
  local len = #s
  while i <= len do
    local ch = s:sub(i, i)
    if ch ~= "\\" then
      result[#result + 1] = ch
      i = i + 1
    else
      local next = s:sub(i + 1, i + 1)
      local mapped = ESCAPE_MAP[next]
      if mapped then
        result[#result + 1] = mapped
        i = i + 2
      elseif next == "" then
        result[#result + 1] = "\\"
        i = i + 1
      else
        result[#result + 1] = "\\"
        result[#result + 1] = next
        i = i + 2
      end
    end
  end
  return table.concat(result)
end

local function normalize_whitespace(s)
  local result = {}
  local prev_ws = false
  for pos, cp in utf8.codes(s) do
    local is_ws = cp == 0x20
      or (cp >= 0x09 and cp <= 0x0D)
      or cp == 0x85
      or cp == 0xA0
      or cp == 0x1680
      or (cp >= 0x2000 and cp <= 0x200A)
      or cp == 0x2028
      or cp == 0x2029
      or cp == 0x202F
      or cp == 0x205F
      or cp == 0x3000
    if is_ws then
      if not prev_ws and #result > 0 then
        result[#result + 1] = " "
      end
      prev_ws = true
    else
      prev_ws = false
      local next_pos = utf8.offset(s, 2, pos)
      if next_pos then
        result[#result + 1] = s:sub(pos, next_pos - 1)
      else
        result[#result + 1] = s:sub(pos)
      end
    end
  end
  local r = table.concat(result)
  if r:sub(-1) == " " then
    r = r:sub(1, -2)
  end
  return r
end

local function strip_common_indent(lines)
  local min_indent = math.huge
  for _, l in ipairs(lines) do
    local trimmed = trim(l)
    if trimmed ~= "" then
      local indent = #l - #l:match("^%s*(.*)")
      if indent < min_indent then
        min_indent = indent
      end
    end
  end
  if min_indent == math.huge then
    min_indent = 0
  end

  local result = {}
  for _, l in ipairs(lines) do
    local trimmed = trim(l)
    if trimmed == "" then
      result[#result + 1] = l
    else
      result[#result + 1] = l:sub(min_indent + 1)
    end
  end
  return table.concat(result, "\n")
end

local function codepoints(s)
  local cps = {}
  for _, cp in utf8.codes(s) do
    cps[#cps + 1] = cp
  end
  return cps
end

local function levenshtein(a, b)
  local a_cps = codepoints(a)
  local b_cps = codepoints(b)
  local a_len = #a_cps
  local b_len = #b_cps
  if a_len == 0 then
    return b_len
  end
  if b_len == 0 then
    return a_len
  end

  local prev = {}
  for j = 0, b_len do
    prev[j] = j
  end
  local curr = {}

  for i = 1, a_len do
    curr[0] = i
    local ca = a_cps[i]
    for j = 1, b_len do
      local cost = (ca == b_cps[j]) and 0 or 1
      local ins = prev[j] + 1
      local del = curr[j - 1] + 1
      local sub = prev[j - 1] + cost
      if ins < del then
        curr[j] = ins < sub and ins or sub
      else
        curr[j] = del < sub and del or sub
      end
    end
    prev, curr = curr, prev
  end
  return prev[b_len]
end

local function substring_whitespace_match(line, normalized_find)
  local normalized_line = normalize_whitespace(line)
  if not normalized_line:find(normalized_find, 1, true) or normalized_line == normalized_find then
    return nil
  end

  local words = split_lines(normalized_find:gsub(" ", "\n"))
  if #words == 0 then
    return nil
  end

  local escaped = {}
  for _, w in ipairs(words) do
    escaped[#escaped + 1] = escape_pattern(w)
  end
  local pattern = table.concat(escaped, "%s+")
  local s, e = line:find(pattern)
  if s then
    return line:sub(s, e)
  end
  return nil
end

local function middle_similarity(block, search)
  local block_mid = math.max(0, #block - 2)
  local search_mid = math.max(0, #search - 2)
  local lines_to_check = math.min(block_mid, search_mid)
  if lines_to_check == 0 then
    return 1.0
  end

  local total = 0
  for j = 1, lines_to_check do
    local a = trim(block[j + 1])
    local b = trim(search[j + 1])
    local max_len = math.max(utf8.len(a) or #a, utf8.len(b) or #b)
    if max_len == 0 then
      total = total + 1.0
    else
      total = total + (1.0 - levenshtein(a, b) / max_len)
    end
  end
  return total / lines_to_check
end

local function exact(_content, find)
  return { find }
end

local function line_trimmed(content, find)
  local content_lines = split_lines(content)
  local search_lines = split_lines(find)
  if #search_lines > 0 and search_lines[#search_lines] == "" then
    search_lines[#search_lines] = nil
  end
  if #search_lines == 0 or #search_lines > #content_lines then
    return {}
  end

  local results = {}
  for i = 1, #content_lines - #search_lines + 1 do
    local all_match = true
    for j = 1, #search_lines do
      local cl = trim(content_lines[i + j - 1])
      local sl = trim(search_lines[j])
      if cl ~= sl then
        all_match = false
        break
      end
    end
    if all_match then
      results[#results + 1] = join_slice(content_lines, i, #search_lines)
    end
  end
  return results
end

local function indentation_flexible(content, find)
  local find_lines = split_lines(find)
  local content_lines = split_lines(content)
  if #find_lines == 0 or #find_lines > #content_lines then
    return {}
  end

  local normalized_find = strip_common_indent(find_lines)
  local results = {}

  for i = 1, #content_lines - #find_lines + 1 do
    local block = table_slice(content_lines, i, #find_lines)
    if strip_common_indent(block) == normalized_find then
      results[#results + 1] = table.concat(block, "\n")
    end
  end
  return results
end

local function trimmed_boundary(content, find)
  local trimmed = trim(find)
  if trimmed == find then
    return {}
  end

  local results = {}
  if content:find(trimmed, 1, true) then
    results[#results + 1] = trimmed
  end

  local find_lines = split_lines(find)
  local content_lines = split_lines(content)
  if #find_lines > 1 and #find_lines <= #content_lines then
    for i = 1, #content_lines - #find_lines + 1 do
      local joined = join_slice(content_lines, i, #find_lines)
      if trim(joined) == trimmed then
        results[#results + 1] = joined
      end
    end
  end
  return results
end

local function block_anchor(content, find)
  local content_lines = split_lines(content)
  local search_lines = split_lines(find)
  if #search_lines > 0 and search_lines[#search_lines] == "" then
    search_lines[#search_lines] = nil
  end
  if #search_lines < CONTEXT_AWARE_LINE_MIN then
    return {}
  end

  local first_trimmed = trim(search_lines[1])
  local last_trimmed = trim(search_lines[#search_lines])

  local candidates = {}
  for i = 1, #content_lines do
    if trim(content_lines[i]) == first_trimmed then
      local tail_start = i + 2
      if tail_start <= #content_lines then
        for j = tail_start, #content_lines do
          if trim(content_lines[j]) == last_trimmed then
            candidates[#candidates + 1] = { i, j }
            break
          end
        end
      end
    end
  end

  if #candidates == 0 then
    return {}
  end

  if #candidates == 1 then
    local c = candidates[1]
    local count = c[2] - c[1] + 1
    local block = table_slice(content_lines, c[1], count)
    local sim = middle_similarity(block, search_lines)
    if sim >= SINGLE_CANDIDATE_THRESHOLD then
      return { table.concat(block, "\n") }
    end
    return {}
  end

  local best_block, best_sim = nil, -1.0
  for _, c in ipairs(candidates) do
    local count = c[2] - c[1] + 1
    local block = table_slice(content_lines, c[1], count)
    local sim = middle_similarity(block, search_lines)
    if sim > best_sim then
      best_block, best_sim = block, sim
    end
  end

  if best_sim >= MULTI_CANDIDATE_THRESHOLD then
    return { table.concat(best_block, "\n") }
  end
  return {}
end

local function whitespace_normalized(content, find)
  local normalized_find = normalize_whitespace(find)
  local content_lines = split_lines(content)
  local results = {}

  for _, line in ipairs(content_lines) do
    if normalize_whitespace(line) == normalized_find then
      results[#results + 1] = line
    else
      local matched = substring_whitespace_match(line, normalized_find)
      if matched then
        results[#results + 1] = matched
      end
    end
  end

  local find_lines = split_lines(find)
  if #find_lines > 1 and #find_lines <= #content_lines then
    for i = 1, #content_lines - #find_lines + 1 do
      local joined = join_slice(content_lines, i, #find_lines)
      if normalize_whitespace(joined) == normalized_find then
        results[#results + 1] = joined
      end
    end
  end

  return results
end

local function escape_normalized(content, unescaped_find)
  local results = {}
  if content:find(unescaped_find, 1, true) then
    results[#results + 1] = unescaped_find
  end

  local content_lines = split_lines(content)
  local find_lines = split_lines(unescaped_find)
  if #find_lines > 1 and #find_lines <= #content_lines then
    for i = 1, #content_lines - #find_lines + 1 do
      local joined = join_slice(content_lines, i, #find_lines)
      if unescape(joined) == unescaped_find then
        results[#results + 1] = joined
      end
    end
  end

  return results
end

local function context_aware(content, find)
  local content_lines = split_lines(content)
  local find_lines = split_lines(find)
  if #find_lines > 0 and find_lines[#find_lines] == "" then
    find_lines[#find_lines] = nil
  end
  if #find_lines < CONTEXT_AWARE_LINE_MIN then
    return {}
  end

  local first_trimmed = trim(find_lines[1])
  local last_trimmed = trim(find_lines[#find_lines])
  local results = {}

  for i = 1, #content_lines do
    if trim(content_lines[i]) == first_trimmed then
      local e = i + #find_lines - 1
      if e <= #content_lines and trim(content_lines[e]) == last_trimmed then
        local matching, total = 0, 0
        for k = 2, #find_lines - 1 do
          local bl = trim(content_lines[i + k - 1])
          local fl = trim(find_lines[k])
          if bl ~= "" or fl ~= "" then
            total = total + 1
            if bl == fl then
              matching = matching + 1
            end
          end
        end

        if total == 0 or matching / total >= CONTEXT_AWARE_MATCH_RATIO then
          results[#results + 1] = join_slice(content_lines, i, #find_lines)
        end
      end
    end
  end

  return results
end

local REPLACERS = { exact, line_trimmed, block_anchor, whitespace_normalized, indentation_flexible }
local LATE_REPLACERS = { trimmed_boundary, context_aware }

-- An empty candidate matches at every offset and would splice forever, so it
-- is made to match nowhere instead.
local function occurrences(content, matched)
  local at = {}
  if matched == "" then
    return at
  end
  local pos = 1
  while true do
    local s = content:find(matched, pos, true)
    if not s then
      return at
    end
    at[#at + 1] = s
    pos = s + #matched
  end
end

local function splice(content, at, len, replacement)
  local out, pos = {}, 1
  for _, s in ipairs(at) do
    out[#out + 1] = content:sub(pos, s - 1)
    out[#out + 1] = replacement
    pos = s + len
  end
  out[#out + 1] = content:sub(pos)
  return table.concat(out)
end

-- Indentation only means something for a block that starts its own line; a match
-- landing mid line keeps whatever the model wrote.
local function all_start_a_line(content, at)
  for _, s in ipairs(at) do
    if s > 1 and content:sub(s - 1, s - 1) ~= "\n" then
      return false
    end
  end
  return true
end

-- Replace {old_string} with {new_string} in {content}, tolerating small
-- whitespace and indentation drift. Returns the new content, or nil plus
-- one of the error constants above.
function M.replace(content, old_string, new_string, replace_all)
  if old_string == "" then
    return nil, M.EMPTY_OLD_STRING
  end

  local any_found = false

  local function try_match(candidates, find, replacement)
    for _, matched in ipairs(candidates) do
      local at = occurrences(content, matched)
      if #at > 0 then
        any_found = true
        if replace_all or #at == 1 then
          local text = replacement
          if all_start_a_line(content, at) then
            text = reindent(matched, find, replacement)
          end
          return splice(content, at, #matched, text)
        end
      end
    end
    return nil
  end

  for _, r in ipairs(REPLACERS) do
    local res = try_match(r(content, old_string), old_string, new_string)
    if res then
      return res, nil
    end
  end

  local unescaped = unescape(old_string)
  if unescaped ~= old_string then
    local res = try_match(escape_normalized(content, unescaped), unescaped, unescape(new_string))
    if res then
      return res, nil
    end
  end

  for _, r in ipairs(LATE_REPLACERS) do
    local res = try_match(r(content, old_string), old_string, new_string)
    if res then
      return res, nil
    end
  end

  if any_found then
    return nil, M.MULTIPLE_MATCHES
  end
  return nil, M.NO_MATCH
end

return M
