-- Output formatting for the Bazel indexer.
--
-- Knows nothing about Starlark, tree-sitter, or the AST layer. Operates only
-- on the record shapes produced by analyze.lua and on plain strings, so it can
-- be unit-tested or repurposed without dragging in tree-sitter.

local DEFAULT_INDENT_LEVEL = 1
local INDENT_WIDTH = 2

return function(U)
  local format_range = U.format_range

  local R = {}

  local function render_lines(header_line, items, render_fn)
    local lines = { header_line }

    for _, item in ipairs(items) do
      lines[#lines + 1] = render_fn(item)
    end

    return table.concat(lines, "\n") .. "\n"
  end

  local function indent(level)
    return (" "):rep(INDENT_WIDTH * level)
  end

  local function join_line(...)
    local fields = {}

    for i = 1, select("#", ...) do
      local part = select(i, ...)

      if part and part ~= "" then
        fields[#fields + 1] = part
      end
    end

    return table.concat(fields, " ")
  end

  function R.append_section_if_present(sections, section)
    if section then
      sections[#sections + 1] = section
    end
  end

  function R.render_items(header, items, render_fn)
    if #items == 0 then
      return nil
    end

    return render_lines(header .. ":", items, render_fn)
  end

  function R.item_range(item)
    return format_range(item.line_start, item.line_end)
  end

  function R.item_line(value, line_range, opts)
    local indent_level = opts and opts.indent or DEFAULT_INDENT_LEVEL

    return join_line(indent(indent_level) .. value, line_range)
  end

  function R.label_line(label, value, line_range, opts)
    local indent_level = opts and opts.indent or DEFAULT_INDENT_LEVEL

    return join_line(indent(indent_level) .. label .. ":", value, line_range)
  end

  function R.render_doc(kind, collected)
    if not collected.doc then
      return nil
    end

    return kind .. " doc: " .. format_range(collected.doc[1], collected.doc[2]) .. "\n"
  end

  return R
end
