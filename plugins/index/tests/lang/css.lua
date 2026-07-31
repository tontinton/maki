local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("css_rule_sets_and_media_queries", function()
  local src = [==[
.card {
  color: red;
}

@media (max-width: 600px) {
  .card {
    padding: 4px;
  }
  #nav {
    display: none;
  }
}
]==]
  local out = idx(src, "css")
  has(out, {
    "rules:",
    "  .card [1-3]",
    "  @media (max-width: 600px) [5-12]",
    "    .card [6-8]",
    "    #nav [9-11]",
  })
  lacks(out, {
    "color: red",
    "padding",
    "display: none",
  })
end)

case("css_imports", function()
  local src = "@import 'base.css';\n.a { color: red; }\n"
  local out = idx(src, "css")
  has(out, {
    "imports: [1]",
    "  'base.css'",
    "  .a [2]",
  })
end)

case("css_keyframes_without_bodies", function()
  local src = "@keyframes spin {\n  from { transform: rotate(0); }\n  to { transform: rotate(360deg); }\n}\n"
  local out = idx(src, "css")
  has(out, {
    "rules:",
    "  @keyframes spin [1-4]",
  })
  lacks(out, {
    "transform",
  })
end)

case("css_at_rule_without_block_and_nested_rules", function()
  local src = "@layer base, components;\n.card {\n  color: red;\n  & .title { font-weight: bold; }\n}\n"
  local out = idx(src, "css")
  has(out, {
    "  @layer base, components; [1]",
    "  .card [2-5]",
    "    & .title [4]",
  })
  lacks(out, {
    "color: red",
  })
end)

case("css_ranged_meta", function()
  local src = ".card { color: red; }\n"
  local out, meta = helpers.idx_with_meta(src, "css")
  helpers.assert_ranged_meta(out, meta, {
    ".card",
  })
end)
