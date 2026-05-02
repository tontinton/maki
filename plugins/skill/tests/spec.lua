local helpers = require("skill_helpers")
local parse_frontmatter = helpers.parse_frontmatter
local build_skill_list = helpers.build_skill_list

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

case("no_frontmatter_returns_content_as_body", function()
  local fm, body = parse_frontmatter("Just content without frontmatter")
  eq(fm.name, nil)
  eq(body, "Just content without frontmatter")
end)

case("frontmatter_with_leading_whitespace", function()
  local fm, body = parse_frontmatter("  \n---\nname: trimmed\n---\nBody here")
  eq(fm.name, "trimmed")
  eq(body, "Body here")
end)

case("frontmatter_no_closing_delimiter", function()
  local input = "---\nname: oops\nThis never closes"
  local fm, body = parse_frontmatter(input)
  eq(fm.name, nil)
  eq(body, input)
end)

case("frontmatter_invalid_yaml_falls_back", function()
  local fm, body = parse_frontmatter("---\n: invalid: yaml: [[\n---\nBody")
  eq(fm.name, nil)
  eq(body, "Body")
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

case("frontmatter_only_dashes_no_yaml", function()
  local fm, body = parse_frontmatter("---\n\n---\nBody")
  eq(body, "Body")
end)

-- ── build_skill_list ──

case("build_skill_list_empty", function()
  local result = build_skill_list({})
  assert(result:find("No skills available"), "empty list should say no skills available")
  assert(result:find("<available_skills>"), "should have opening tag")
  assert(result:find("</available_skills>"), "should have closing tag")
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

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
