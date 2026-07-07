local shorten_path = require("maki.shorten_path")

local DESCRIPTION =
  [[View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `read` for images.

- Paths: absolute, relative, or ~/.
- Oversized images are downscaled automatically (animated gif/webp keep only the first frame).]]

-- Anthropic rejects images over 5MB base64; 3MB raw is ~4MB encoded,
-- which leaves headroom.
local MAX_RAW_BYTES = 3 * 1024 * 1024
-- Anthropic downscales anything over 1568px on the long edge server-side
-- anyway, so ship fewer bytes and do it here.
local MAX_EDGE = 1568
-- Refuse absurdly large files up front; maki.image.decode also enforces a
-- host-side pixel cap against decode bombs.
local MAX_INPUT_BYTES = 50 * 1024 * 1024

local MEDIA_TYPES = {
  png = "image/png",
  jpeg = "image/jpeg",
  gif = "image/gif",
  webp = "image/webp",
}

local function format_size(bytes)
  if bytes >= 1024 * 1024 then
    return string.format("%.1fMB", bytes / (1024 * 1024))
  end
  return string.format("%dKB", math.ceil(bytes / 1024))
end

local function caption(path, bytes, width, height, note)
  -- Shortened path, not basename: two screenshot.png in different dirs must
  -- stay distinguishable when several images land in one turn.
  return string.format("[image: %s %s %dx%d%s]", shorten_path(path), format_size(bytes), width, height, note or "")
end

local function fail(msg)
  return { llm_output = msg, is_error = true }
end

local function load_image(path)
  local bytes, read_err = maki.fs.read_bytes(path)
  if not bytes then
    return fail("cannot read " .. path .. ": " .. (read_err or "unknown error"))
  end
  local size = buffer.len(bytes)
  if size > MAX_INPUT_BYTES then
    return fail(
      string.format("%s is too large to view (%s; limit %s)", path, format_size(size), format_size(MAX_INPUT_BYTES))
    )
  end

  local info, probe_err = maki.image.probe(bytes)
  if not info then
    return fail(path .. " is not an image (" .. (probe_err or "unrecognized format") .. ")")
  end
  local media_type = MEDIA_TYPES[info.format]
  if not media_type then
    return fail("unsupported image format " .. info.format .. ": only png, jpeg, gif, and webp can be viewed")
  end

  -- Decode fully even on the pass-through path: a corrupt file shipped
  -- undecoded poisons message history and fails every later request.
  local img, decode_err = maki.image.decode(bytes)
  if not img then
    return fail("cannot decode " .. path .. ": " .. (decode_err or "unknown error"))
  end

  if size <= MAX_RAW_BYTES and math.max(info.width, info.height) <= MAX_EDGE then
    return {
      llm_output = caption(path, size, info.width, info.height),
      image = { media_type = media_type, data = maki.base64.encode(bytes) },
    }
  end

  -- Too big for the API: downscale to fit MAX_EDGE and re-encode. JPEG stays
  -- JPEG (photos recompress far smaller); everything else becomes PNG since
  -- gif/webp encoding isn't supported.
  local resized = math.max(info.width, info.height) > MAX_EDGE
  if resized then
    img = img:resize(MAX_EDGE, MAX_EDGE)
  end

  local out_format = info.format == "jpeg" and "jpeg" or "png"
  local encoded = img:encode(out_format)
  if #encoded > MAX_RAW_BYTES and out_format == "png" then
    -- PNG can stay huge at 1568px (e.g. noisy screenshots); JPEG is the only
    -- remaining lever.
    out_format = "jpeg"
    encoded = img:encode(out_format)
  end
  if #encoded > MAX_RAW_BYTES then
    return fail(
      string.format(
        "%s is too large to view (%s after downscaling; limit %s)",
        path,
        format_size(#encoded),
        format_size(MAX_RAW_BYTES)
      )
    )
  end

  local note = resized and string.format(", downscaled from %dx%d", info.width, info.height) or ", re-encoded"
  -- Animated gif/webp lose their animation when re-encoded.
  if info.format == "gif" or info.format == "webp" then
    note = note .. ", first frame only"
  end

  return {
    llm_output = caption(path, #encoded, img:width(), img:height(), note),
    image = { media_type = MEDIA_TYPES[out_format], data = maki.base64.encode(encoded) },
  }
end

maki.api.register_tool({
  name = "view_image",
  kind = "read",
  description = DESCRIPTION,
  -- No interpreter audience: the code_execution bridge flattens tool output
  -- to text, so the pixels could never reach the model from there.
  audiences = { "main", "research_sub", "general_sub" },

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Path to the image file",
        required = true,
        alias = "file_path",
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    buf:line({ { shorten_path(input.path or ""), "path" } })
    return buf
  end,

  handler = function(input, _ctx)
    local raw = input.path
    if not raw then
      return fail("error: path is required")
    end
    local path = maki.fs.abspath(raw)
    local meta = maki.fs.metadata(path)
    if not meta then
      return fail("error: path not found: " .. path)
    end
    if meta.is_dir then
      return fail("error: " .. path .. " is a directory")
    end
    return load_image(path)
  end,
})
