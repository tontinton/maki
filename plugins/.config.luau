return {
  luau = {
    languagemode = "nonstrict",
    -- `luau-analyze` reports `#` on untyped strings as `TableOperations`, and `ImplicitReturn` is style.
    lint = { TableOperations = false, ImplicitReturn = false },
  },
  lute = {
    check = { definitions = { maki = "./maki.d.luau" } },
    lint = {
      rulepaths = { "./lint" },
      ruleconfigs = {
        -- Test setup calls fail the test anyway, at the assertion that needed them.
        dropped_maki_error = { ignores = { "**/tests/**", "lib/maki/test_helpers.lua" } },
      },
    },
  },
}
