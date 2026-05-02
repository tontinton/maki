local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("markdown_atx_headings", function()
  local src =
    "# Main Title\n\n## Section 1\n\nSome text here.\n\n### Subsection 1.1\n\nMore content.\n\n## Section 2\n\n### Another Subsection 2.1\n\n#### Deep Heading 2.1.3\n\n# Footer Title\n\nAnd some more content\n"
  local out = idx(src, "markdown")
  has(out, {
    "headings:",
    "# Main Title [1-16]",
    "## Section 1 [3-10]",
    "### Subsection 1.1 [7-10]",
    "## Section 2 [11-16]",
    "### Another Subsection 2.1 [13-16]",
    "#### Deep Heading 2.1.3 [15-16]",
    "# Footer Title [17-20]",
  })
  lacks(out, { "Some text here", "More content", "And some more content" })
end)

case("markdown_atx_headings_no_newline", function()
  local src = "# Main Title\nSome text here\n# Footer Title"
  local out = idx(src, "markdown")
  has(out, { "headings:", "# Main Title [1-2]", "# Footer Title [3]" })
  lacks(out, { "Some text here" })
end)

case("markdown_setext_headings", function()
  local src =
    "Heading 1\n=========\n\nSome text here\n\nHeading 1.1\n---------\n\nMore content\n\nHeading 2\n=========\n\nAnd some more content\n"
  local out = idx(src, "markdown")
  has(out, {
    "headings:",
    "# Heading 1 [1-10]",
    "## Heading 1.1 [6-10]",
    "# Heading 2 [11-15]",
  })
  lacks(out, { "Some text here", "More content", "And some more content" })
end)
