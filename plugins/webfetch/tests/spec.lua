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

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
