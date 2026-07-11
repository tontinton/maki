-- Exercises the maki.image / maki.base64 primitives view_image is built
-- from. The full tool path is covered by the view_image tests in
-- maki-lua/tests/plugin_host.rs.

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    failures[#failures + 1] = name .. ": " .. tostring(err)
  end
end

local TINY_PNG_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="

case("base64_roundtrip", function()
  local bytes = maki.base64.decode(TINY_PNG_B64)
  assert(maki.base64.encode(bytes) == TINY_PNG_B64, "roundtrip mismatch")
end)

case("probe_reports_png_dimensions", function()
  local bytes = maki.base64.decode(TINY_PNG_B64)
  local info, err = maki.image.probe(bytes)
  assert(err == nil, err)
  assert(info.format == "png", "format: " .. tostring(info.format))
  assert(info.width == 1 and info.height == 1, info.width .. "x" .. info.height)
end)

case("probe_rejects_non_image", function()
  local info, err = maki.image.probe("definitely not an image")
  assert(info == nil, "expected nil info")
  assert(err ~= nil, "expected an error")
end)

case("decode_rejects_truncated_image", function()
  local bytes = maki.base64.decode(TINY_PNG_B64)
  local img, err = maki.image.decode(bytes:sub(1, math.floor(#bytes / 2)))
  assert(img == nil, "expected nil image")
  assert(err ~= nil, "expected an error")
end)

case("decode_resize_encode_pipeline", function()
  local bytes = maki.base64.decode(TINY_PNG_B64)
  local img, err = maki.image.decode(bytes)
  assert(err == nil, err)
  assert(img:width() == 1 and img:height() == 1)

  -- resize never upscales
  local same = img:resize(100, 100)
  assert(same:width() == 1 and same:height() == 1)

  local png = img:encode("png")
  local info = maki.image.probe(png)
  assert(info.format == "png")

  local jpeg = img:encode("jpeg")
  local jinfo = maki.image.probe(jpeg)
  assert(jinfo.format == "jpeg")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
