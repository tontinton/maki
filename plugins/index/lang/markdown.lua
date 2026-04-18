return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local new_entry = U.new_entry
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION

  local function heading_level(node, source)
    if node:type() == "atx_heading" then
      local text = get_text(node, source)
      local count = 0
      for i = 1, #text do
        if text:sub(i, i) == "#" then count = count + 1 else break end
      end
      return count
    else
      local text = get_text(node, source)
      return text:match("=+%s*$") and 1 or 2
    end
  end

  local function collect_all_headings(entries, node, source)
    local kind = node:type()
    if kind == "atx_heading" or kind == "setext_heading" then
      local content_node = node:field("heading_content")[1]
      if content_node then
        local level = heading_level(node, source)
        local text = get_text(content_node, source):match("^%s*(.-)%s*$")
        local prefix = string.rep("#", level)
        local entry = new_entry(SECTION.Heading, node, compact_ws(prefix .. " " .. text))
        entries[#entries + 1] = { level = level, entry = entry }
      end
    end
    for _, child in ipairs(node:children()) do
      collect_all_headings(entries, child, source)
    end
  end

  return {
    extract = function(source, root)
      local tagged = {}
      collect_all_headings(tagged, root, source)

      local doc_end = root:end_() + 1
      for i, item in ipairs(tagged) do
        local end_line = doc_end
        for j = i + 1, #tagged do
          if tagged[j].level <= item.level then
            end_line = tagged[j].entry.line_start - 1
            break
          end
        end
        item.entry.line_end = end_line
      end

      local entries = {}
      for _, item in ipairs(tagged) do
        entries[#entries + 1] = item.entry
      end

      return format_skeleton(entries, {}, nil, "")
    end,
  }
end
