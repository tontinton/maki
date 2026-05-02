local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("bash_all_sections", function()
  local src = [==[
#!/bin/bash

MAX_RETRIES=5
LOG_DIR="/var/log"

my_func() {
    echo "hello"
}

function process() {
    echo "processing"
}
]==]
  local out = idx(src, "bash")
  has(out, {
    "consts:",
    "MAX_RETRIES = 5",
    "LOG_DIR",
    "fns:",
    "my_func()",
    "process()",
  })
end)
