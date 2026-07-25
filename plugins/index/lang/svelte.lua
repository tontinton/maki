return function(U)
  return require("lang.simple_outline")(U, {
    header = "components",
    nodes = {
      script_element = { label = "script: " },
      style_element = { label = "style: " },
      element = { label = "markup: " },
      snippet_statement = { label = "snippet: " },
    },
  })
end
