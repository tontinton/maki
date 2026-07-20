local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has
local lacks = helpers.lacks

case("toml_top_level_pair", function()
  local src = [==[
    title = "TOML Example"
    version = 1
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "consts:",
    "title",
    "TOML Example",
    "version",
  })
end)

case("toml_table_with_pairs", function()
  local src = [==[
    [package]
    name = "maki"
    version = "0.3.27"
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "consts:",
    "[package]",
    "name",
    "maki",
    "version",
    "0.3.27",
  })
end)

case("toml_table_array_element", function()
  local src = [==[
    [[bin]]
    name = "maki"
    path = "src/main.rs"
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "[[bin]]",
    "name",
    "maki",
    "path",
    "src/main.rs",
  })
end)

case("toml_dotted_keys", function()
  local src = [==[
    a.b.c = 1

    [package.metadata.docs]
    rs = true
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "a.b.c",
    "[package.metadata.docs]",
    "rs",
  })
end)

case("toml_inline_table_and_array_values", function()
  local src = [==[
    [settings]
    tags = ["a", "b", "c"]
    meta = { x = 1, y = 2 }
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "[settings]",
    "tags = [",
    "meta = {",
  })
end)

case("toml_comments_ignored", function()
  local src = [==[
    # top comment
    [server]
    # inline comment
    host = "localhost" # trailing comment
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "[server]",
    "host",
    "localhost",
  })
  lacks(out, {
    "top comment",
    "inline comment",
    "trailing comment",
  })
end)

case("toml_table_field_truncation", function()
  local src = "[data]\n"
  for i = 1, 9 do
    src = src .. "k" .. i .. " = " .. i .. "\n"
  end
  local out = idx(src, "toml")
  has(out, { "[data]", "k1 = 1\n", "k8 = 8\n", "k9\n" })
  lacks(out, { "k9 = 9", "[1 more truncated]" })
end)

case("toml_truncation_at_exact_threshold", function()
  local src = "[data]\n"
  for i = 1, 8 do
    src = src .. "k" .. i .. " = " .. i .. "\n"
  end
  local out = idx(src, "toml")
  has(out, { "[data]", "k1 = 1\n", "k8 = 8\n" })
end)

case("toml_quoted_and_dotted_keys", function()
  local src = [==[
    ["quoted.section"]
    "weird.key" = 1
    'single.quoted' = 2
  ]==]
  local out = idx(src, "toml")
  has(out, {
    '"quoted.section"',
    "weird.key",
    "single.quoted",
  })
end)

case("toml_empty_table_and_top_level_pairs", function()
  local src = [==[
    top = "value"

    [empty]

    [next]
    x = 1
  ]==]
  local out = idx(src, "toml")
  has(out, { 'top = "value"', "[empty]", "[next]", "x" })
end)

case("toml_array_of_tables_multiple_elements", function()
  local src = [==[
    [[products]]
    name = "widget"

    [[products]]
    name = "gadget"
  ]==]
  local out = idx(src, "toml")
  has(out, {
    "[[products]]",
    "widget",
    "gadget",
  })
end)
