local LINE_FMT = "%4d | %s"
local LINE_PAT = "^%s*%d+ | (.*)$"

local M = {}

function M.parse_frontmatter(content)
  local rest = content:match("^%s*%-%-%-\n(.*)")
  if not rest then
    return {}, content
  end
  local end_pos = rest:find("\n%-%-%-")
  if not end_pos then
    return {}, content
  end
  local yaml_str = rest:sub(1, end_pos)
  local body = rest:sub(end_pos + 4):match("^%s*(.-)%s*$")
  local fm, _ = maki.yaml.decode(yaml_str)
  if not fm then
    fm = {}
  end
  return fm, body
end

function M.build_skill_list(skills)
  local sorted = {}
  for _, s in pairs(skills) do
    sorted[#sorted + 1] = s
  end
  table.sort(sorted, function(a, b)
    return a.name < b.name
  end)

  if #sorted == 0 then
    return "\n\n<available_skills>\nNo skills available.\n</available_skills>"
  end

  local lines = {}
  for _, s in ipairs(sorted) do
    lines[#lines + 1] = "- " .. s.name .. ": " .. s.description
  end
  return "\n\n<available_skills>\n" .. table.concat(lines, "\n") .. "\n</available_skills>"
end

-- `parse_output` must stay the exact inverse: the view is drawn from
-- this text, live and on restore.
function M.format_output(location, content)
  local lines = { location }
  for i, line in ipairs(maki.split(content, "\n")) do
    lines[i + 1] = string.format(LINE_FMT, i, line)
  end
  return table.concat(lines, "\n")
end

function M.parse_output(output)
  local location, rest = output:match("^([^\n]*)\n(.*)$")
  if not location then
    return nil
  end
  local lines = {}
  for i, line in ipairs(maki.split(rest, "\n")) do
    lines[i] = line:match(LINE_PAT)
    if not lines[i] then
      return nil
    end
  end
  return location, table.concat(lines, "\n")
end

return M
