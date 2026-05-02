local SKILL_FILE = "SKILL.md"
local NOT_FOUND = "skill not found: "
local shorten_path = require("shorten_path")
local ToolView = require("tool_view")
local helpers = require("skill_helpers")
local parse_frontmatter = helpers.parse_frontmatter
local build_skill_list = helpers.build_skill_list

local PROJECT_SKILL_DIRS = {
  ".maki/skills",
  ".claude/skills",
  ".opencode/skills",
  ".agents/skills",
}
local GLOBAL_SKILL_DIRS = {
  ".maki/skills",
  ".claude/skills",
  ".config/opencode/skills",
  ".agents/skills",
}

local function scan_skill_dir(dir, skills)
  local entries = maki.fs.dir(dir)
  if not entries then
    return
  end
  for _, entry in ipairs(entries) do
    if entry[2] == "directory" then
      local skill_path = maki.fs.joinpath(dir, entry[1], SKILL_FILE)
      local ok, content = pcall(maki.fs.read, skill_path)
      if ok and content then
        local fm, body = parse_frontmatter(content)
        if body and #body > 0 then
          local name = (fm and fm.name) or entry[1]
          skills[name] = {
            name = name,
            description = (fm and fm.description) or "",
            content = body,
            location = skill_path,
          }
        end
      end
    end
  end
end

local function find_project_ancestors()
  local cwd = maki.uv.cwd()
  local dirs = { cwd }
  for _, parent in ipairs(maki.fs.parents(cwd)) do
    dirs[#dirs + 1] = parent
    local git = maki.fs.joinpath(parent, ".git")
    if maki.fs.metadata(git) then
      break
    end
  end
  return dirs
end

local function discover_skills()
  local skills = {}
  local home = maki.uv.os_homedir()

  if home then
    for _, rel in ipairs(GLOBAL_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(home, rel), skills)
    end
  end

  for _, ancestor in ipairs(find_project_ancestors()) do
    for _, rel in ipairs(PROJECT_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(ancestor, rel), skills)
    end
  end

  return skills
end

local boot_skills = discover_skills()
local description = "Load a skill that provides instructions and workflows for specific tasks."
  .. build_skill_list(boot_skills)

maki.api.register_tool({
  name = "skill",
  description = description,

  schema = {
    type = "object",
    properties = {
      name = { type = "string", description = "Name of the skill to load", required = true },
    },
  },

  header = function(input)
    return input.name
  end,

  handler = function(input, ctx)
    if not input.name then
      return "error: name is required"
    end

    local skills = discover_skills()
    local skill = skills[input.name]
    if not skill then
      local available = build_skill_list(skills)
      return NOT_FOUND .. input.name .. available
    end

    local lines = {}
    local i = 1
    for line in (skill.content .. "\n"):gmatch("([^\n]*)\n") do
      lines[#lines + 1] = string.format("%4d | %s", i, line)
      i = i + 1
    end
    local formatted = skill.location .. "\n" .. table.concat(lines, "\n")

    local buf = maki.ui.buf()
    local tol = ctx:tool_output_lines()
    local view = ToolView.new(buf, {
      max_lines = (tol and tol.skill) or 20,
      keep = "head",
    })
    buf:on("click", function()
      view:toggle()
    end)

    local ext = skill.location:match("%.([^%.]+)$") or "md"
    local highlighted = maki.ui.highlight(skill.content, ext)
    if highlighted then
      for idx, hl_line in ipairs(highlighted) do
        local spans = { { string.format("%4d ", idx), "line_nr" } }
        for _, seg in ipairs(hl_line) do
          spans[#spans + 1] = seg
        end
        view:append(spans)
      end
    else
      for line in formatted:gmatch("([^\n]*)\n?") do
        view:append(line)
      end
    end
    view:finish()

    local short = shorten_path(skill.location)
    local header_buf = maki.ui.buf()
    header_buf:line({ { short, "path" } })

    return {
      llm_output = formatted,
      body = buf,
      header = header_buf,
    }
  end,
})
