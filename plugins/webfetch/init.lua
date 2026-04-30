local VALID_FORMATS = { markdown = true, text = true, html = true }
local DEFAULT_FORMAT = "markdown"
local SKIP_TAGS = { script = true, style = true, noscript = true }
local ACCEPT_HEADERS = {
  html = "text/html,*/*;q=0.5",
  text = "text/plain,text/html;q=0.9,*/*;q=0.5",
  markdown = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.5",
}

local LT, GT, SLASH, SPACE, TAB, CR, LF = 60, 62, 47, 32, 9, 13, 10
local function is_ws(b)
  return b == SPACE or b == TAB or b == CR or b == LF
end

local function strip_html(html)
  local out = {}
  local in_tag = false
  local tag_start = 0
  local skip_tag = nil
  local last_was_space = true

  for i = 1, #html do
    local b = html:byte(i)
    if b == LT then
      in_tag = true
      tag_start = i + 1
    elseif b == GT then
      in_tag = false
      local tag_str = html:sub(tag_start, i - 1):lower()
      local tag_name = tag_str:match("^%s*(%S+)")

      if skip_tag then
        if tag_name and tag_name:byte(1) == SLASH and tag_name:sub(2) == skip_tag then
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
      -- accumulate nothing; we use sub(tag_start, i-1) on close
    elseif not skip_tag then
      if is_ws(b) then
        if not last_was_space and #out > 0 then
          out[#out + 1] = " "
          last_was_space = true
        end
      else
        out[#out + 1] = html:sub(i, i)
        last_was_space = false
      end
    end
  end

  local result = table.concat(out)
  return result:match("^%s*(.-)%s*$")
end

local truncate = require("truncate")
local ToolView = require("tool_view")

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
      url = { type = "string", description = "URL to fetch (http:// or https://)", required = true },
      format = { type = "string", description = "Output format: markdown (default), text, or html" },
      timeout = { type = "integer", description = "Timeout in seconds (default 30, max 120)" },
    },
  },
  permission_scope = "url",

  header = function(input)
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
      headers = {
        ["Accept"] = ACCEPT_HEADERS[fmt],
      },
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

    local llm_output = truncate(body, max_lines, max_bytes)

    local tol = ctx:tool_output_lines()
    local buf = maki.ui.buf()
    local view = ToolView.new(buf, {
      max_lines = (tol and tol.web) or 3,
      keep = "head",
    })
    buf:on("click", function()
      view:toggle()
    end)

    for line in body:gmatch("([^\n]*)\n?") do
      view:append(line)
    end
    view:finish()

    return {
      llm_output = llm_output,
      body = buf,
    }
  end,
})
