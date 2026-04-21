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

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

-- ── strip_html ──

case("strip_html_nested_tags_and_whitespace", function()
  eq(strip_html("<div><p>Hello <b>world</b></p></div>"), "Hello world")
  eq(strip_html("   <p>  lots   of    spaces  </p>   "), "lots of spaces")
  eq(strip_html("<p>line1\n\n\nline2</p>"), "line1 line2")
end)

case("strip_html_skip_tags", function()
  eq(strip_html("before<script>alert('xss')</script>after"), "before after")
  eq(strip_html("before<style>.a{color:red}</style>after"), "before after")
  eq(strip_html("before<noscript>enable js</noscript>after"), "before after")
  eq(strip_html("a<SCRIPT>evil()</SCRIPT>b"), "a b")
  eq(strip_html("a<script>var x = '<div>not real</div>';</script>b"), "a b")
end)

case("strip_html_mixed_content", function()
  eq(strip_html("<p>keep</p><script>drop</script><p>also keep</p>"), "keep also keep")
  eq(strip_html("<td>cell1</td><td>cell2</td>"), "cell1 cell2")
  eq(strip_html('<a href="http://example.com" class="link">click</a>'), "click")
  eq(strip_html("before<br/>after"), "before after")
end)

case("strip_html_edge_cases", function()
  eq(strip_html(""), "")
  eq(strip_html("<div><span></span></div>"), "")
  eq(strip_html("hello<div"), "hello")
end)

-- ── truncate ──

case("truncate_within_limits_unchanged", function()
  eq(truncate("hello", 100, 1000), "hello")
  eq(truncate("a\nb\nc", 3, 1000), "a\nb\nc")
  eq(truncate("", 100, 1000), "")
end)

case("truncate_exceeds_line_limit", function()
  local result = truncate("aaa\nbbb\nccc\nddd", 2, 1000)
  assert(result:find("aaa", 1, true), "should keep first line")
  assert(result:find("bbb", 1, true), "should keep second line")
  assert(not result:find("ccc", 1, true), "should drop third line")
  assert(result:find("%[truncated %d+ bytes%]"), "should have truncation marker")
end)

case("truncate_exceeds_byte_limit", function()
  local text = string.rep("x", 200)
  local result = truncate(text, 1000, 50)
  assert(#result < #text, "should be shorter")
  assert(result:find("%[truncated"), "should have truncation marker")
end)

case("truncate_byte_limit_mid_line", function()
  local text = "short\n" .. string.rep("x", 100)
  local result = truncate(text, 1000, 20)
  assert(result:find("short"), "should keep first line")
  assert(not result:find(string.rep("x", 100)), "should drop long line")
  assert(result:find("%[truncated"), "should have truncation marker")
end)

case("truncate_trailing_newlines_counted", function()
  local result = truncate("a\n\n\n\n\n", 2, 1000)
  assert(result:find("%[truncated"), "trailing newlines should count as lines")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
