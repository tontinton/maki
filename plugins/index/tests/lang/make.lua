local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("make_targets_without_recipes", function()
  local src =
    "build: src/main.rs\n\tcargo build --release\n\tstrip target/release/bin\n\ninstall: build\n\tcp target/release/bin /usr/local/bin\n"
  local out = idx(src, "make")
  has(out, {
    "targets:",
    "  build: [1-3]",
    "  install: [5-6]",
  })
  lacks(out, {
    "src/main.rs",
    "cargo build",
    "strip",
    "cp target",
  })
end)

case("make_variables_and_includes", function()
  local src = "BIN := app\nCFLAGS = -O2 -Wall\n\ninclude config.mk\n"
  local out = idx(src, "make")
  has(out, {
    "consts:",
    "  BIN := app [1]",
    "  CFLAGS = -O2 -Wall [2]",
    "imports: [4]",
    "  config.mk",
  })
end)

case("make_define_directive", function()
  local src = "define msg\nhello world\nendef\n"
  local out = idx(src, "make")
  has(out, {
    "consts:",
    "  define msg [1-3]",
  })
  lacks(out, {
    "hello world",
  })
end)

case("make_conditional_branches_are_flat", function()
  local src = "CFLAGS = -O2\nifeq ($(DEBUG),1)\nCFLAGS += -g\ndebug: build\n\techo debug\nelse\nCFLAGS += -O3\nendif\n"
  local out = idx(src, "make")
  has(out, {
    "consts:",
    "  CFLAGS = -O2 [1]",
    "  CFLAGS += -g [3]",
    "  CFLAGS += -O3 [7]",
    "targets:",
    "  debug: [4-5]",
  })
  lacks(out, {
    "echo debug",
    "    CFLAGS",
  })
end)

case("make_nested_and_tab_indented_conditionals", function()
  local src =
    "ifdef TLS\n\tTLS_MODULE := yes\n\t@echo tls on\n\tifeq ($(SSL),0)\n\t\tSSL_LIBS = -lssl\n\tendif\nelse ifdef NOTLS\nTLS_MODULE := no\nendif\n"
  local out = idx(src, "make")
  has(out, {
    "  TLS_MODULE := yes [2]",
    "  SSL_LIBS = -lssl [5]",
    "  TLS_MODULE := no [8]",
  })
  -- Only assignment shaped tab indented lines are variables; shell is noise.
  lacks(out, {
    "echo tls on",
    "ifeq",
    "endif",
  })
end)

case("make_ranged_meta", function()
  local src = "build:\n\tcargo build\n"
  local out, meta = helpers.idx_with_meta(src, "make")
  helpers.assert_ranged_meta(out, meta, {
    "build:",
  })
end)
