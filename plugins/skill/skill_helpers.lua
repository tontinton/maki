local LINE_FMT = "%4d | %s"
local LINE_PAT = "^%s*%d+ | (.*)$"

local SKILL_OPEN = "<skill_content>\n"
local SKILL_CLOSE = "\n</skill_content>"
local MAX_NAME_LENGTH = 64
local MAX_DESCRIPTION_LENGTH = 1024
local MAX_COMPATIBILITY_LENGTH = 500
local STRING_METADATA_FIELDS = {
  name = true,
  description = true,
  compatibility = true,
  license = true,
  ["allowed-tools"] = true,
}

local M = {}

local function quote_plain_metadata(yaml)
  return yaml:gsub("[^\r\n]+", function(line)
    local key, value = line:match("^([%a][%w%-]*):[ \t]+(.*)$")
    if not STRING_METADATA_FIELDS[key] then
      return line
    end
    value = value:gsub("[ \t]+#.*$", ""):gsub("[ \t]+$", "")
    if value:find(":[ \t]") and not value:find("^[\"'%[%]{}>|&*!%%@`#]") then
      local quoted = maki.json.encode(value)
      if quoted then
        return key .. ": " .. quoted
      end
    end
    return line
  end)
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
  local fm, err = maki.yaml.decode(yaml_str)
  if err then
    local quoted = quote_plain_metadata(yaml_str)
    if quoted ~= yaml_str then
      fm, err = maki.yaml.decode(quoted)
    end
  end
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

function M.validate_frontmatter(fm, directory)
  for _, field in ipairs({ "name", "description" }) do
    if type(fm[field]) ~= "string" or not fm[field]:find("%S") then
      return nil, field .. " must be a non-empty string"
    end
  end
  if utf8.len(fm.description) > MAX_DESCRIPTION_LENGTH then
    return nil, "description exceeds " .. MAX_DESCRIPTION_LENGTH .. " characters"
  end
  local compatibility: string? = fm.compatibility
  if compatibility ~= nil then
    if type(compatibility) ~= "string" or not compatibility:find("%S") then
      return nil, "compatibility must be a non-empty string"
    end
    if utf8.len(compatibility) > MAX_COMPATIBILITY_LENGTH then
      return nil, "compatibility exceeds " .. MAX_COMPATIBILITY_LENGTH .. " characters"
    end
  end
  for _, field in ipairs({ "license", "allowed-tools" }) do
    if fm[field] ~= nil and type(fm[field]) ~= "string" then
      return nil, field .. " must be a string"
    end
  end
  if fm.metadata ~= nil then
    if type(fm.metadata) ~= "table" then
      return nil, "metadata must be a string-to-string mapping"
    end
    for key, value in pairs(fm.metadata) do
      if type(key) ~= "string" or type(value) ~= "string" then
        return nil, "metadata must be a string-to-string mapping"
      end
    end
  end
  local warnings = {}
  local name = fm.name
  if utf8.len(name) > MAX_NAME_LENGTH then
    warnings[#warnings + 1] = "name exceeds " .. MAX_NAME_LENGTH .. " characters"
  end
  if name:find("[^a-z0-9%-]") or name:find("^%-") or name:find("%-$") or name:find("%-%-") then
    warnings[#warnings + 1] = "name should use lowercase letters, digits, and single hyphens"
  end
  if name ~= directory then
    warnings[#warnings + 1] = "name does not match directory " .. directory
  end
  return warnings
end

function M.scan_skill_dir(dir, skills)
  local diagnostics, skipped = {}, {}
  local entries = maki.fs.dir(dir) or {}
  table.sort(entries, function(a, b)
    return a[1] < b[1]
  end)
  for _, entry in ipairs(entries) do
    if entry[2] == "directory" then
      local path = maki.fs.joinpath(dir, entry[1], "SKILL.md")
      local content = maki.fs.read(path)
      if content then
        local fm, _, err = M.parse_frontmatter(content)
        local warnings
        if fm then
          warnings, err = M.validate_frontmatter(fm, entry[1])
        end
        if err then
          local diagnostic = path .. ": " .. err
          diagnostics[#diagnostics + 1] = diagnostic
          skipped[#skipped + 1] = diagnostic
        else
          for _, warning in ipairs(warnings) do
            diagnostics[#diagnostics + 1] = path .. ": " .. warning
          end
          local previous = skills[fm.name]
          if previous and previous.location ~= path then
            diagnostics[#diagnostics + 1] = path .. " shadows " .. previous.location .. " (" .. fm.name .. ")"
          end
          skills[fm.name] = {
            name = fm.name,
            description = fm.description,
            content = content,
            location = path,
          }
        end
      end
    end
  end
  return diagnostics, skipped
end

function M.skill_names(skills)
  local names = {}
  for name in pairs(skills) do
    names[#names + 1] = name
  end
  table.sort(names)
  return names
end

function M.build_skill_list(skills)
  local names = M.skill_names(skills)
  if #names == 0 then
    return ""
  end
  local lines = {}
  for _, name in ipairs(names) do
    local skill = skills[name]
    lines[#lines + 1] = "- " .. skill.name .. ": " .. skill.description
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

function M.activation_output(skill)
  local directory = not skill.location:match("^builtin:") and maki.fs.dirname(skill.location)
  local content = skill.content
  if directory then
    content = content .. "\n\nSkill directory: " .. directory .. "\nResolve relative paths from this directory."
  end
  return SKILL_OPEN .. M.format_output(skill.location, content) .. SKILL_CLOSE
end

function M.parse_output(output)
  if output:sub(1, #SKILL_OPEN) == SKILL_OPEN and output:sub(-#SKILL_CLOSE) == SKILL_CLOSE then
    output = output:sub(#SKILL_OPEN + 1, -#SKILL_CLOSE - 1)
  end
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
