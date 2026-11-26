local VALID_FORMATS = { markdown = true, text = true, html = true }
local DEFAULT_FORMAT = "markdown"
local SKIP_TAGS = { "script", "style", "noscript" }
local MARKDOWN_SKIP_TAGS = { "script", "style", "noscript", "iframe", "object", "svg", "template" }
local CONTENT_ROOT_TAGS = { "main" }
local PROSE_TYPES = { "text/html", "text/markdown", "text/plain" }
-- `read` cuts a line at 500 bytes by default, and a paragraph is one line.
local WRAP_BYTES = 400
local FENCE = "^%s*[`~][`~][`~]"
local HEADING = "^#+ "
local UNWRAPPED = "^[|#]"
local NO_SESSION_ERR = "the call has no session"
local PAGE_EXT = ".md"
local PAGE_NAME_BYTES = 100
local PREVIEW_LINES = 20
local PREVIEW_BYTES = 2048
local SAVED_OUTLINE =
  "Page saved to %s (%d bytes, %d lines). Read the part you need with offset and limit. Headings by line:\n%s"
local SAVED_PREVIEW =
  "Page saved to %s (%d bytes, %d lines). Read the part you need with offset and limit. It starts with:\n%s"
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
  max_inline_bytes = {
    default = 16 * 1024,
    min = 1024,
    desc = "A page longer than this is saved to a file, and the model gets its outline to read from.",
  },
}))

local function web_view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.web) or 3, keep = "head" }
end

local function is_prose(content_type: string): boolean
  for _, prose_type in ipairs(PROSE_TYPES) do
    if content_type:find(prose_type, 1, true) then
      return true
    end
  end
  return false
end

-- Breaks long lines at spaces so `read` shows all of them, and notes where
-- each heading lands. Code blocks, table rows and headings stay as they are.
local function lay_out(text: string): ({ string }, { string })
  local lines: { string }, headings: { string } = {}, {}
  local fenced = false
  for line in text:gmatch("([^\n]*)\n?") do
    if line:find(FENCE) then
      fenced = not fenced
    elseif not fenced and line:find(HEADING) then
      headings[#headings + 1] = ("%d: %s"):format(#lines + 1, line)
    end
    if not fenced and not line:find(UNWRAPPED) then
      while #line > WRAP_BYTES do
        local space = line:sub(1, WRAP_BYTES):match("^.*() ")
        if not space or space == 1 then
          break
        end
        lines[#lines + 1] = line:sub(1, space - 1)
        line = line:sub(space + 1)
      end
    end
    lines[#lines + 1] = line
  end
  return lines, headings
end

-- Returns the reply that stands in for a saved page, or nil plus an error.
local function save_page(
  session: string?,
  url: string,
  text: string,
  max_lines: number?,
  max_bytes: number
): (string?, string?)
  if not session then
    return nil, NO_SESSION_ERR
  end
  local dir, dir_err = maki.session.data_dir({ session = session })
  if not dir then
    return nil, dir_err
  end
  local made, mkdir_err = maki.fs.mkdir(dir, { parents = true })
  if not made then
    return nil, mkdir_err
  end
  local name = url:gsub("^%a+://", ""):gsub("[^%w_%-%.]", "_"):sub(1, PAGE_NAME_BYTES)
  local path = maki.fs.joinpath(dir, name .. PAGE_EXT)
  local lines, headings = lay_out(text)
  local page = table.concat(lines, "\n")
  local written, write_err = maki.fs.write(path, page)
  if not written then
    return nil, write_err
  end
  if #headings > 0 then
    local outline = truncate(table.concat(headings, "\n"), max_lines, max_bytes)
    return SAVED_OUTLINE:format(path, #page, #lines, outline)
  end
  return SAVED_PREVIEW:format(path, #page, #lines, truncate(page, PREVIEW_LINES, PREVIEW_BYTES))
end

maki.api.register_tool({
  name = "webfetch",
  kind = "fetch",
  description = [[Fetch a URL and return its contents.

- Supports markdown (default), text, or html output formats.
- markdown and text keep only the page's <main> element when it has one, so navigation and footer links need html.
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
      local converted =
        maki.text.html_to_markdown(body, { skip_tags = MARKDOWN_SKIP_TAGS, root_tags = CONTENT_ROOT_TAGS })
      body = converted or body
    elseif fmt == "text" and is_html then
      local stripped = maki.text.html_to_text(body, { skip_tags = SKIP_TAGS, root_tags = CONTENT_ROOT_TAGS })
      body = stripped or body
    end

    local llm_output
    local inline_bytes = max_bytes and math.min(max_bytes, opts.max_inline_bytes)
    if inline_bytes and #body > inline_bytes and fmt ~= "html" and is_prose(ct) then
      local save_err
      llm_output, save_err = save_page(ctx:session_id(), url, body, max_lines, inline_bytes)
      if not llm_output then
        maki.log.warn("webfetch: page not saved, cutting it instead: " .. tostring(save_err))
      end
    end
    llm_output = llm_output or truncate(body, max_lines, max_bytes)

    return {
      llm_output = llm_output,
      body = ToolView.restore(body, web_view_opts(ctx)),
    }
  end,
})
