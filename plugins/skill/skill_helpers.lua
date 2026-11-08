local LINE_FMT = "%4d | %s"
local LINE_PAT = "^%s*%d+ | (.*)$"

local SKILL_FILE = "SKILL.md"
local MAX_NAME_LENGTH = 64
local MAX_DESCRIPTION_LENGTH = 1024
local STRING_METADATA_FIELDS = {
  name = true,
  description = true,
  compatibility = true,
  license = true,
  ["allowed-tools"] = true,
}

local M = {}

M.BUILTIN_PREFIX = "builtin:"

local function decode_yaml(yaml)
  local ok, fm, err = pcall(maki.yaml.decode, yaml)
  if not ok then
    return nil, tostring(fm)
  end
  return fm, err
end

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
  local fm, err = decode_yaml(yaml_str)
  if err then
    local quoted = quote_plain_metadata(yaml_str)
    if quoted ~= yaml_str then
      fm, err = decode_yaml(quoted)
    end
  end
  if err then
    return nil, nil, "invalid YAML: " .. err
  end
  if type(fm) ~= "table" then
    return nil, nil, "frontmatter must be a mapping"
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
  for _, entry in ipairs(maki.fs.dir(dir) or {}) do
    if entry[2] == "directory" then
      local path = maki.fs.joinpath(dir, entry[1], SKILL_FILE)
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

function M.activation_output(skill)
  local directory = skill.location:sub(1, #M.BUILTIN_PREFIX) ~= M.BUILTIN_PREFIX and maki.fs.dirname(skill.location)
  local content = skill.content
  if directory then
    content = content .. "\n\nSkill directory: " .. directory .. "\nResolve relative paths from this directory."
  end
  return M.format_output(skill.location, content)
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
