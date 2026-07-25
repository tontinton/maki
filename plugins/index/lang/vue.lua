return function(U)
  return require("lang.simple_outline")(U, {
    header = "components",
    nodes = {
      script_element = { label = "script: " },
      style_element = { label = "style: " },
      template_element = { label = "template: " },
    },
  })
end
