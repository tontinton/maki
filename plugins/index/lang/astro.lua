return function(U)
  return require("lang.simple_outline")(U, {
    header = "components",
    nodes = {
      frontmatter = { label = "frontmatter: " },
      script_element = { label = "script: " },
      style_element = { label = "style: " },
      element = { label = "markup: " },
    },
  })
end
