local helpers = require("tests.helpers")
local case = helpers.case
local has = helpers.has
local idx = helpers.idx
local indexer = require("indexer")

case("astro_indexes_common_structure", function()
  local source = "---\nconst title = 'Hello'\n---\n<main id=\"page\"><h1>{title}</h1></main>"
  has(idx(source, "astro"), { "components:", "frontmatter:", "markup:" })
end)

case("svelte_indexes_common_structure", function()
  local source = "<script>let count = 0;</script>\n<main>{count}</main>\n<style>main { color: red; }</style>"
  has(idx(source, "svelte"), { "components:", "script:", "markup:", "style:" })
end)

case("vue_indexes_common_structure", function()
  local source = "<template><main>{{ title }}</main></template>\n<script setup>const title = 'Hi'</script>"
  has(idx(source, "vue"), { "components:", "template:", "script:" })
end)

case("astro_svelte_vue_map_extensions", function()
  assert(indexer.EXT_TO_LANG.astro == "astro")
  assert(indexer.EXT_TO_LANG.svelte == "svelte")
  assert(indexer.EXT_TO_LANG.vue == "vue")
end)
