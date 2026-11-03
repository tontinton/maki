local helpers = require("skill_helpers")
local parse_frontmatter = helpers.parse_frontmatter
local build_skill_list = helpers.build_skill_list
local format_output = helpers.format_output
local parse_output = helpers.parse_output

local LOCATION = "/home/u/.config/maki/skills/release/SKILL.md"

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

-- ── parse_frontmatter ──

case("frontmatter_with_name_and_description", function()
  local fm, body =
    parse_frontmatter("---\nname: git-release\ndescription: Create releases\n---\n## Instructions\nDo stuff")
  eq(fm.name, "git-release")
  eq(fm.description, "Create releases")
  assert(body:find("Instructions"), "body should contain content after closing ---")
end)

case("frontmatter_with_leading_whitespace", function()
  local fm, body = parse_frontmatter("  \n---\nname: trimmed\n---\nBody here")
  eq(fm.name, "trimmed")
  eq(body, "Body here")
end)

case("frontmatter_recovers_plain_metadata_with_colons", function()
  for _, newline in ipairs({ "\n", "\r\n" }) do
    local fm, body, err = parse_frontmatter(table.concat({
      "---",
      "name: portable",
      'description: Use when: the user asks about "PDFs" # parser comment',
      "compatibility: Requires: Python 3",
      "license: Custom: see LICENSE",
      "allowed-tools: Bash(command: *)",
      "metadata:",
      '  author: "Team: docs"',
      "---",
      "Body: keep this unchanged",
    }, newline))
    eq(err, nil)
    eq(fm.description, 'Use when: the user asks about "PDFs"')
    eq(fm.compatibility, "Requires: Python 3")
    eq(fm.license, "Custom: see LICENSE")
    eq(fm["allowed-tools"], "Bash(command: *)")
    eq(fm.metadata.author, "Team: docs")
    eq(body, "Body: keep this unchanged")
  end
end)

case("invalid_frontmatter_returns_a_diagnostic", function()
  for _, input in ipairs({
    "Plain instructions",
    "---\nname: missing-close",
    "---\n: invalid: yaml: [[\n---\nBody",
    "---\n123\n---\nBody",
    "---\nname: {nested: value}\n---\nBody",
    "---\nname: valid\ndescription: [wrong]\n---\nBody",
    "---\nname: valid\ndescription: [unclosed: value\n---\nBody",
    "---\nname: valid\ndescription: Use when: " .. string.char(255) .. "\n---\nBody",
  }) do
    local fm, body, err = parse_frontmatter(input)
    eq(fm, nil)
    eq(body, nil)
    assert(type(err) == "string" and #err > 0, "invalid files need a diagnostic")
  end
end)

case("frontmatter_empty_body_after_close", function()
  local fm, body = parse_frontmatter("---\nname: x\n---\n   ")
  eq(fm.name, "x")
  eq(body, "")
end)

case("frontmatter_body_with_embedded_triple_dashes", function()
  local fm, body = parse_frontmatter("---\nname: tricky\n---\nSome text\n---\nMore text")
  eq(fm.name, "tricky")
  assert(body:find("Some text"), "body should start after first closing ---")
end)

case("frontmatter_accepts_crlf_and_complete_delimiters", function()
  for _, newline in ipairs({ "\n", "\r\n" }) do
    local input = table.concat({
      "---",
      "name: portable",
      "description: Windows file",
      "---extra: value",
      "---",
      "Body",
    }, newline)
    local fm, body = parse_frontmatter(input)
    eq(fm.name, "portable")
    eq(fm["---extra"], "value")
    eq(body, "Body")
  end
end)

case("frontmatter_closing_delimiter_at_eof", function()
  local fm, body = parse_frontmatter("---\nname: portable\ndescription: Example\n---")
  eq(fm.name, "portable")
  eq(body, "")
end)

case("required_metadata_and_character_limits", function()
  for _, fm in ipairs({
    {},
    { name = "valid" },
    { name = "", description = "Example" },
    { name = "valid", description = "  " },
    { name = "valid", description = string.rep("x", 1025) },
    { name = "valid", description = "Example", compatibility = "" },
    { name = "valid", description = "Example", compatibility = string.rep("x", 501) },
  }) do
    local warnings, err = helpers.validate_frontmatter(fm, "valid")
    eq(warnings, nil)
    assert(err and #err > 0, "invalid metadata needs an error")
  end
  local fm = { name = "valid", description = string.rep("é", 1024), compatibility = string.rep("é", 500) }
  local warnings, err = helpers.validate_frontmatter(fm, "valid")
  eq(err, nil)
  eq(#warnings, 0)
end)

case("cosmetic_name_issues_load_with_diagnostics", function()
  for _, name in ipairs({ "Upper", "-start", "end-", "two--hyphens", "has space", string.rep("x", 65) }) do
    local warnings, err = helpers.validate_frontmatter({ name = name, description = "Example" }, "other")
    eq(err, nil)
    assert(#warnings > 0, "name violations must be reported")
  end
end)

case("discovery_keeps_valid_empty_body_and_skips_bad_neighbors", function()
  local th = require("maki.test_helpers")
  local dir = th.mktmpdir("skill_discovery")
  local ok, err = pcall(function()
    for name, content in pairs({
      valid = "---\nname: valid\ndescription: Example\n---\n",
      broken = "---\n123\n---\nBody",
      missing = "---\nname: missing\n---\nBody",
      portable = "---\nname: portable\ndescription: Use when: testing\n---\nBody",
    }) do
      maki.fs.mkdir(maki.fs.joinpath(dir, name))
      maki.fs.write(maki.fs.joinpath(dir, name, "SKILL.md"), content)
    end
    local skills = {}
    local diagnostics = helpers.scan_skill_dir(dir, skills)
    local _, body = parse_frontmatter(skills.valid.content)
    eq(body, "")
    eq(skills.broken, nil)
    eq(skills.missing, nil)
    eq(skills.portable.description, "Use when: testing")
    eq(#diagnostics, 2)
    for _, message in ipairs(diagnostics) do
      assert(message:find(dir, 1, true), "diagnostic must identify the file")
    end
  end)
  th.rmtree(dir)
  assert(ok, err)
end)

case("collisions_are_deterministic_and_identify_both_files", function()
  local th = require("maki.test_helpers")
  local dir = th.mktmpdir("skill_collisions")
  local ok, err = pcall(function()
    for _, name in ipairs({ "z", "a" }) do
      maki.fs.mkdir(maki.fs.joinpath(dir, name))
      maki.fs.write(maki.fs.joinpath(dir, name, "SKILL.md"), "---\nname: shared\ndescription: Example\n---\n" .. name)
    end
    local skills = {}
    local diagnostics = helpers.scan_skill_dir(dir, skills)
    local _, body = parse_frontmatter(skills.shared.content)
    eq(body, "z")
    local collision
    for _, message in ipairs(diagnostics) do
      if message:find("shadows", 1, true) then
        collision = message
      end
    end
    assert(collision and collision:find(maki.fs.joinpath(dir, "a", "SKILL.md"), 1, true), "identify the shadowed file")
    assert(collision:find(maki.fs.joinpath(dir, "z", "SKILL.md"), 1, true), "identify the winning file")
  end)
  th.rmtree(dir)
  assert(ok, err)
end)

-- ── build_skill_list ──

case("build_skill_list_empty", function()
  local result = build_skill_list({})
  eq(result, "")
end)

case("build_skill_list_single_skill", function()
  local skills = {
    test = { name = "test-skill", description = "A test skill" },
  }
  local result = build_skill_list(skills)
  assert(result:find("test%-skill"), "should contain skill name")
  assert(result:find("A test skill"), "should contain description")
  assert(not result:find("No skills available"), "should not say no skills")
end)

case("build_skill_list_sorted_alphabetically", function()
  local skills = {
    z = { name = "zebra", description = "Z skill" },
    a = { name = "alpha", description = "A skill" },
    m = { name = "middle", description = "M skill" },
  }
  local result = build_skill_list(skills)
  local alpha_pos = result:find("alpha")
  local middle_pos = result:find("middle")
  local zebra_pos = result:find("zebra")
  assert(alpha_pos < middle_pos, "alpha should come before middle")
  assert(middle_pos < zebra_pos, "middle should come before zebra")
end)

case("optional_metadata_types_are_checked", function()
  for field, value in pairs({ license = {}, compatibility = {}, metadata = { version = 1 }, ["allowed-tools"] = {} }) do
    local fm = { name = "release", description = "Example" }
    fm[field] = value
    local warnings, err = helpers.validate_frontmatter(fm, "release")
    eq(warnings, nil)
    assert(err and err:find(field, 1, true), "diagnostic must name the field")
  end
end)

case("activation_retains_metadata_and_relative_path_base", function()
  local source =
    "---\nname: release\ndescription: Example\nlicense: MIT\ncompatibility: Requires python3\nmetadata:\n  version: '1'\nallowed-tools: Bash(git:*) Read\n---\n# Release"
  local output = helpers.activation_output({ location = LOCATION, content = source })
  local location, content = parse_output(output)
  eq(location, LOCATION)
  assert(content:find(source, 1, true), "activation must retain all metadata")
  assert(content:find(maki.fs.dirname(LOCATION), 1, true), "activation must identify the resource base")
  assert(output:find("<skill_content>", 1, true), "activation needs a durable content boundary")
end)

-- ── llm text round trip ──

case("format_output_numbers_each_line_under_location", function()
  eq(format_output(LOCATION, "# Release\nTag it."), LOCATION .. "\n   1 | # Release\n   2 | Tag it.")
end)

for name, content in pairs({
  blank_lines = "a\n\n\nb",
  trailing_newline = "a\nb\n",
  single_line = "only",
  looks_like_format = "   1 | nested\n  12 | more",
  many_lines = string.rep("x\n", 12000) .. "end",
}) do
  case("parse_output_inverts_format_output_" .. name, function()
    local location, parsed = parse_output(format_output(LOCATION, content))
    eq(location, LOCATION)
    eq(parsed, content)
  end)
end

case("parse_output_rejects_text_that_is_not_a_listing", function()
  eq(parse_output("error: name is required"), nil)
  eq(parse_output("skill not found: x\n\n<available_skills>\n- a: b\n</available_skills>"), nil)
end)

-- ── builtin plugin_dev skill ──

case("builtin_plugin_dev_skill_loads", function()
  local builtin = require("plugin_dev")
  eq(builtin.name, "maki-plugin-dev")
  assert(#builtin.description > 0, "description should not be empty")
  assert(builtin.content:find("# Writing maki plugins", 1, true), "content should contain the authoring guide")
  assert(#builtin.reference_placeholder > 0, "reference_placeholder should not be empty")
  assert(
    builtin.content:find(builtin.reference_placeholder, 1, true),
    "content should contain the reference path placeholder"
  )
  assert(builtin.content:find("maki.api.register_tool", 1, true), "index should list register_tool")
end)

case("builtin_plugin_dev_reference_loads", function()
  local ref = require("plugin_dev_reference")
  assert(ref.content:find("### `maki.api.register_tool", 1, true), "reference should document register_tool")
  assert(ref.content:find("Shared helper modules", 1, true), "reference should include helper modules")
end)

case("builtin_index_line_numbers_match_reference", function()
  local builtin = require("plugin_dev")
  local ref = require("plugin_dev_reference")
  local lines = maki.split(ref.content, "\n")
  local checked = 0
  for ln, sig in builtin.content:gmatch("\n%- L(%d+) `([^`]+)`") do
    local target = lines[tonumber(ln)]
    local heading = "### `" .. sig .. "`"
    assert(
      target and target:find(heading, 1, true) == 1,
      "index L" .. ln .. " should point at " .. heading .. ", got: " .. tostring(target)
    )
    checked = checked + 1
  end
  assert(checked > 100, "expected the index to cover the reference, checked " .. checked)
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
