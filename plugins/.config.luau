return {
  luau = {
    languagemode = "nonstrict",
    -- `luau-analyze` reports `#` on untyped strings as `TableOperations`, and `ImplicitReturn` is style.
    lint = { TableOperations = false, ImplicitReturn = false },
  },
  lute = { check = { definitions = { maki = "./maki.d.luau" } } },
}
