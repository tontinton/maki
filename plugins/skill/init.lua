local NOT_FOUND = "skill not found: "
local REFERENCE_FILE = "lua-api.md"
local REFERENCE_UNAVAILABLE = "(unavailable; full reference inlined below)"
local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")
local helpers = require("skill_helpers")
local build_skill_list = helpers.build_skill_list
local parse_output = helpers.parse_output

local PROJECT_SKILL_DIRS = {
  ".maki/skills",
  ".claude/skills",
  ".opencode/skills",
  ".agents/skills",
}
local GLOBAL_SKILL_DIRS = {
  ".claude/skills",
  ".config/opencode/skills",
  ".agents/skills",
}

local function scan_skill_dir(dir, skills, skipped)
  local diagnostics, invalid = helpers.scan_skill_dir(dir, skills)
  for _, diagnostic in ipairs(diagnostics) do
    maki.log.warn(diagnostic)
  end
  if skipped then
    table.move(invalid, 1, #invalid, #skipped + 1, skipped)
  end
end

local opts = maki.api.register_options({
  plugin_dev = { default = true, desc = "Offer the builtin maki-plugin-dev skill for writing maki plugins." },
})

local ok, builtin, reference = pcall(function()
  return require("plugin_dev"), require("plugin_dev_reference")
end)
if not ok then
  maki.log.warn("builtin plugin_dev skill unavailable: " .. tostring(builtin))
  builtin = nil
end

local function resolve_builtin_content()
  local state = maki.env.state_dir()
  if state then
    local dir = maki.fs.joinpath(state, "docs")
    local path = maki.fs.joinpath(dir, REFERENCE_FILE)
    local _, err = maki.fs.mkdir(dir, { parents = true })
    if not err then
      _, err = maki.fs.write(path, reference.content)
    end
    if not err then
      return (builtin.content:gsub(builtin.reference_placeholder, function()
        return path
      end))
    end
    maki.log.warn("failed to write lua api reference to " .. path .. ": " .. tostring(err))
  end
  local content = builtin.content:gsub(builtin.reference_placeholder, REFERENCE_UNAVAILABLE)
  return content .. "\n---\n\n" .. reference.content
end

local function discover_skills(skipped)
  local skills = {}

  if builtin and opts.plugin_dev then
    skills[builtin.name] = {
      name = builtin.name,
      description = builtin.description,
      content = builtin.content,
      location = helpers.BUILTIN_PREFIX .. builtin.name,
      resolve = resolve_builtin_content,
    }
  end

  local config = maki.env.config_dir()
  if config then
    scan_skill_dir(maki.fs.joinpath(config, "skills"), skills, skipped)
  end

  local home = maki.uv.os_homedir()
  if home then
    for _, rel in ipairs(GLOBAL_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(home, rel), skills, skipped)
    end
  end

  for _, ancestor in ipairs(maki.env.project_dirs()) do
    for _, rel in ipairs(PROJECT_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(ancestor, rel), skills, skipped)
    end
  end

  return skills
end

local function view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 3, keep = "head" }
end

-- Live call and restore both draw from the llm text, so the two views
-- cannot drift. Nil when the text is not a skill listing.
local function skill_view(output, ctx)
  local location, content = parse_output(output)
  if not location then
    return nil
  end

  local buf = maki.ui.buf()
  local view = ToolView.new(buf, view_opts(ctx))
  buf:on("click", function()
    view:toggle()
  end)
  view:set_highlight(content, location:match("%.([^%.]+)$") or "md")
  view:finish()

  local header = maki.ui.buf()
  header:line({ { shorten_path(location), "path" } })
  return { body = buf, header = header }
end

local skipped = {}
local boot_skills = discover_skills(skipped)
for _, diagnostic in ipairs(skipped) do
  maki.notify(diagnostic, "warn", { title = "Skipped skill" })
end
local names = helpers.skill_names(boot_skills)
if #names == 0 then
  return
end
local description = "When a task matches a listed skill, load it before proceeding." .. build_skill_list(boot_skills)

maki.api.register_tool({
  name = "skill",
  kind = "read",
  durable = "name",
  description = description,
  permission = "fs_read",

  schema = {
    type = "object",
    properties = {
      name = { type = "string", enum = names, description = "Name of the skill to load", required = true },
    },
  },

  header = function(input)
    return input.name
  end,

  restore = function(_input, output, _is_error, ctx)
    return skill_view(output, ctx) or ToolView.restore(output, view_opts(ctx))
  end,

  handler = function(input, ctx)
    if not input.name then
      return { llm_output = "error: name is required", is_error = true }
    end

    local skills = discover_skills()
    local skill = skills[input.name]
    if not skill then
      local available = build_skill_list(skills)
      return { llm_output = NOT_FOUND .. input.name .. available, is_error = true }
    end
    if skill.resolve then
      skill.content = skill.resolve()
    end

    local llm_output = helpers.activation_output(skill)
    local result = skill_view(llm_output, ctx)
    result.llm_output = llm_output
    return result
  end,
})
