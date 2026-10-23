local LINE_FMT = "%4d | %s"
local LINE_PAT = "^%s*%d+ | (.*)$"

local M = {}

local function decode_yaml(yaml)
  local ok, fm, err = pcall(maki.yaml.decode, yaml)
  if not ok then
    return nil, tostring(fm)
  end
  return fm, err
end

function M.parse_frontmatter(content)
  local rest = content:match("^%s*%-%-%-[ \t]*\r?\n(.*)")
  if not rest then
    return nil, nil, "missing YAML frontmatter"
  end
  local end_pos, end_line = (rest .. "\n"):find("\n%-%-%-[ \t]*\r?\n")
  if not end_pos then
    return nil, nil, "missing closing frontmatter delimiter"
  end
  local yaml_str = rest:sub(1, end_pos)
  local body = rest:sub(end_line + 1):match("^%s*(.-)%s*$")
  local fm, err = decode_yaml(yaml_str)
  if err then
    return nil, nil, "invalid YAML: " .. err
  end
  if type(fm) ~= "table" then
    return nil, nil, "frontmatter must be a mapping"
  end
  for _, field in ipairs({ "name", "description" }) do
    if fm[field] ~= nil and type(fm[field]) ~= "string" then
      return nil, nil, field .. " must be a string"
    end
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
