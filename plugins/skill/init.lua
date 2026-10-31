local SKILL_TOOL = "skill"
local SKILL_COMMAND = "/" .. SKILL_TOOL
local SKILL_USAGE = "Usage: /skill name [request]"
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
      location = "builtin:" .. builtin.name,
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

local function load_skill(name)
  local skill = discover_skills()[name]
  if not skill then
    return nil, NOT_FOUND .. name
  end
  if skill.resolve then
    skill.content = skill.resolve()
  end
  return helpers.activation_output(skill)
end

local function load_request(args)
  local name, request = args:match("^%s*(%S+)%s*(.*)$")
  if not name then
    return nil, SKILL_USAGE
  end
  local output, err = load_skill(name)
  if not output then
    return nil, err
  end
  local durable = { key = SKILL_TOOL .. ":" .. name, text = output }
  if #request > 0 then
    output = output .. "\n\n" .. request
  end
  return output, nil, durable
end

maki.api.set_slot("agent.user_message", function(prev, value, ctx)
  local updated, reason = prev(value, ctx)
  if type(updated) ~= "table" or type(updated.text) ~= "string" then
    return updated, reason
  end
  local args = updated.text == SKILL_COMMAND and "" or updated.text:match("^" .. SKILL_COMMAND .. "%s+(.*)$")
  if args == nil then
    return updated
  end
  local output, err, durable = load_request(args)
  if not output then
    return nil, err
  end
  updated.text = output
  updated.durable = durable
  return updated
end)

maki.api.register_command({
  name = SKILL_COMMAND,
  nargs = "+",
  description = "Load a skill: /skill name [request]",
  handler = function(command)
    if not command.fargs[1] then
      maki.ui.flash(SKILL_USAGE)
      return
    end
    local session, err = maki.session.current()
    if not session then
      maki.ui.flash(err)
      return
    end
    local state, prompt_error = maki.session.prompt(SKILL_COMMAND .. " " .. command.args, { session = session })
    if not state then
      maki.ui.flash(prompt_error)
    end
  end,
})

local function view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 20, keep = "head" }
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
  name = SKILL_TOOL,
  kind = "read",
  durable = "name",
  description = description,

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

    local llm_output, err = load_skill(input.name)
    if not llm_output then
      return { llm_output = err .. build_skill_list(discover_skills()), is_error = true }
    end
    local result = skill_view(llm_output, ctx)
    result.llm_output = llm_output
    return result
  end,
})
