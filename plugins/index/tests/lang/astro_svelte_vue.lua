local helpers = require("tests.helpers")
local case = helpers.case
local has = helpers.has
local idx = helpers.idx
local indexer = require("indexer")

case("astro_indexes_as_html", function()
  local source = "---\nconst title = 'Hello'\n---\n<main id=\"page\"><h1>{title}</h1></main>"
  has(idx(source, "html"), { "structure:", "<main#page>" })
end)

case("svelte_indexes_as_html", function()
  local source = "<script>let count = 0;</script>\n<main>{count}</main>\n<style>main { color: red; }</style>"
  has(idx(source, "html"), { "structure:", "<script>", "<main>", "<style>" })
end)

case("vue_indexes_as_html", function()
  local source = "<template><main>{{ title }}</main></template>\n<script setup>const title = 'Hi'</script>"
  has(idx(source, "html"), { "structure:", "<template>", "<main>", "<script>" })
end)

case("astro_svelte_vue_map_to_html_extension", function()
  assert(indexer.EXT_TO_LANG.astro == "html")
  assert(indexer.EXT_TO_LANG.svelte == "html")
  assert(indexer.EXT_TO_LANG.vue == "html")
end)
