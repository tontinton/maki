local VALID_FORMATS = { markdown = true, text = true, html = true }
local DEFAULT_FORMAT = "markdown"
local SKIP_TAGS = { "script", "style", "noscript" }
local MARKDOWN_SKIP_TAGS = { "script", "style", "noscript", "iframe", "object", "svg", "template" }
local ACCEPT_HEADERS = {
  html = "text/html,*/*;q=0.5",
  text = "text/plain,text/html;q=0.9,*/*;q=0.5",
  markdown = "text/markdown,text/html;q=0.9,application/xhtml+xml;q=0.9,application/xml;q=0.8,*/*;q=0.5",
}

local truncate = require("maki.truncate")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local opts = maki.api.register_options(output_limits.extend({
  max_response_bytes = {
    default = 5 * 1024 * 1024,
    min = 1024,
    desc = "Stop reading a response after this many bytes, at most 64 MiB.",
  },
}))

local function web_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.web) or 3, keep = "head" }
end

maki.api.register_tool({
  name = "webfetch",
  kind = "fetch",
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
  permission = "net",
  permission_scopes = "url",

  header = function(input)
    local fmt = input.format
    if fmt and fmt ~= DEFAULT_FORMAT then
      return input.url .. " [" .. fmt .. "]"
    end
    return input.url
  end,

  restore = function(_input, output, _is_error, ctx)
    return ToolView.restore(output, web_view_opts(ctx))
  end,

  handler = function(input, ctx)
    local url = input.url
    if not url then
      return { llm_output = "error: url is required", is_error = true }
    end

    local fmt = input.format or DEFAULT_FORMAT
    if not VALID_FORMATS[fmt] then
      return { llm_output = "error: unknown format: " .. tostring(fmt), is_error = true }
    end

    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    local resp, err = maki.net.request(url, {
      timeout = input.timeout or 30,
      max_bytes = opts.max_response_bytes,
      headers = {
        ["Accept"] = ACCEPT_HEADERS[fmt],
      },
    })
    if not resp then
      return { llm_output = "error: " .. tostring(err), is_error = true }
    end

    if resp.status < 200 or resp.status >= 300 then
      return { llm_output = "error: HTTP " .. tostring(resp.status), is_error = true }
    end

    local ct = resp.content_type or ""
    if ct:find("^image/") and not ct:find("svg") then
      return { llm_output = "error: image content cannot be displayed as text", is_error = true }
    end
    -- Text never has NUL, PDFs and zips almost always do. The model can act on this error,
    -- not on a page of U+FFFD.
    if resp.body:find("\0", 1, true) then
      return { llm_output = "error: binary content (" .. ct .. ") cannot be displayed as text", is_error = true }
    end

    -- Decode before truncating, so the cut counts final bytes and not legacy ones that triple.
    local body = maki.text.to_utf8(resp.body)
    local is_html = ct:find("text/html") ~= nil

    if fmt == "markdown" and is_html then
      local converted = maki.text.html_to_markdown(body, { skip_tags = MARKDOWN_SKIP_TAGS })
      body = converted or body
    elseif fmt == "text" and is_html then
      local stripped = maki.text.html_to_text(body, { skip_tags = SKIP_TAGS })
      body = stripped or body
    end

    local llm_output = truncate(body, max_lines, max_bytes)

    return {
      llm_output = llm_output,
      body = ToolView.restore(body, web_view_opts(ctx)),
    }
  end,
})
