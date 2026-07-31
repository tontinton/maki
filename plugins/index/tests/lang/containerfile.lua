local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("containerfile_instructions_in_order", function()
  local src = "# build stage\nFROM rust:1.94 AS build\nWORKDIR /app\nCOPY . .\nRUN cargo build --release\n"
  local out = idx(src, "containerfile")
  has(out, {
    "instructions:",
    "  FROM rust:1.94 AS build [2]",
    "  WORKDIR /app [3]",
    "  COPY . . [4]",
    "  RUN cargo build --release [5]",
  })
  lacks(out, {
    "build stage",
  })
end)

case("containerfile_multi_stage", function()
  local src = 'FROM golang:1.24 AS build\nFROM scratch\nCOPY --from=build /out/app /app\nENTRYPOINT ["/app"]\n'
  local out = idx(src, "containerfile")
  has(out, {
    "  FROM golang:1.24 AS build [1]",
    "  FROM scratch [2]",
    "  COPY --from=build /out/app /app [3]",
    '  ENTRYPOINT ["/app"] [4]',
  })
end)

case("containerfile_ranged_meta", function()
  local out, meta = helpers.idx_with_meta("FROM alpine:3.20\n", "containerfile")
  helpers.assert_ranged_meta(out, meta, {
    "FROM alpine:3.20",
  })
end)
