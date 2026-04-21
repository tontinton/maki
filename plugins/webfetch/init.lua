local VALID_FORMATS = { markdown = true, text = true, html = true }
local DEFAULT_FORMAT = "markdown"
local SKIP_TAGS = { script = true, style = true, noscript = true }

local function strip_html(html)
  local out = {}
  local in_tag = false
  local tag_buf = {}
  local skip_tag = nil
  local last_was_space = true

  for i = 1, #html do
    local ch = html:sub(i, i)
    if ch == "<" then
      in_tag = true
      tag_buf = {}
    elseif ch == ">" then
      in_tag = false
      local tag_str = table.concat(tag_buf):lower()
      local tag_name = tag_str:match("^%s*(%S+)")

      if skip_tag then
        if tag_name and tag_name:sub(1, 1) == "/" and tag_name:sub(2) == skip_tag then
          skip_tag = nil
        end
      elseif tag_name and SKIP_TAGS[tag_name] then
        skip_tag = tag_name
      end

      if not skip_tag and #out > 0 and not last_was_space then
        out[#out + 1] = " "
        last_was_space = true
      end
    elseif in_tag then
      tag_buf[#tag_buf + 1] = ch
    elseif not skip_tag then
      if ch:match("%s") then
        if not last_was_space and #out > 0 then
          out[#out + 1] = " "
          last_was_space = true
        end
      else
        out[#out + 1] = ch
        last_was_space = false
      end
    end
  end

  local result = table.concat(out)
  return result:match("^%s*(.-)%s*$")
end

local function truncate(text, max_lines, max_bytes)
  if #text <= max_bytes then
    local n = 0
    for _ in text:gmatch("\n") do
      n = n + 1
    end
    if n + 1 <= max_lines then
      return text
    end
  end
  local out = {}
  local bytes = 0
  local lines = 0
  for line in text:gmatch("([^\n]*)\n?") do
    lines = lines + 1
    if lines > max_lines then
      break
    end
    local new_bytes = bytes + #line + 1
    if new_bytes > max_bytes then
      break
    end
    out[#out + 1] = line
    bytes = new_bytes
  end
  local result = table.concat(out, "\n")
  if #result < #text then
    result = result .. "\n\n[truncated " .. (#text - #result) .. " bytes]"
  end
  return result
end

maki.api.register_tool({
  name = "webfetch",
  description = [[Fetch a URL and return its contents.

- Supports markdown (default), text, or html output formats.
- HTTP URLs are auto-upgraded to HTTPS.
- Max response size is 5MB, max timeout is 120s.
- Best used inside code_execution with some truncation / filter to avoid context bloat.]],

  schema = {
    type = "object",
    properties = {
      url = { type = "string", description = "URL to fetch (http:// or https://)" },
      format = { type = "string", description = "Output format: markdown (default), text, or html" },
      timeout = { type = "integer", description = "Timeout in seconds (default 30, max 120)" },
    },
    required = { "url" },
  },

  summary = function(input)
    local fmt = input.format
    if fmt and fmt ~= DEFAULT_FORMAT then
      return input.url .. " [" .. fmt .. "]"
    end
    return input.url
  end,

  handler = function(input, ctx)
    local url = input.url
    if not url then
      return "error: url is required"
    end

    local fmt = input.format or DEFAULT_FORMAT
    if not VALID_FORMATS[fmt] then
      return "error: unknown format: " .. tostring(fmt)
    end

    local config = ctx:config()
    local max_response = (config and config.max_response_bytes) or (5 * 1024 * 1024)
    local max_lines = (config and config.max_output_lines) or 2000
    local max_bytes = (config and config.max_output_bytes) or (50 * 1024)

    local resp, err = maki.net.request(url, {
      timeout = input.timeout or 30,
      max_bytes = max_response,
    })
    if not resp then
      return "error: " .. tostring(err)
    end

    if resp.status < 200 or resp.status >= 300 then
      return "error: HTTP " .. tostring(resp.status)
    end

    local ct = resp.content_type or ""
    if ct:find("^image/") and not ct:find("svg") then
      return "error: image content cannot be displayed as text"
    end

    local body = resp.body
    local is_html = ct:find("text/html") ~= nil

    if fmt == "markdown" and is_html then
      local ok, converted = pcall(maki.text.html_to_markdown, body)
      body = ok and converted or body
    elseif fmt == "text" and is_html then
      body = strip_html(body)
    end

    return truncate(body, max_lines, max_bytes)
  end,
})
