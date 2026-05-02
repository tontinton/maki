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

return M
