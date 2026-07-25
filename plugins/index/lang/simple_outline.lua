return function(U, config)
  local compact_ws = U.compact_ws
  local format_range = U.format_range
  local get_text = U.get_text
  local line_end = U.line_end
  local line_start = U.line_start
  local truncate = U.truncate

  local function render(node, source, descriptor)
    local text = compact_ws(get_text(node, source))
    local label
    if type(descriptor.label) == "function" then
      label = descriptor.label(text)
    else
      label = descriptor.label .. truncate(text, config.max_length or 100)
    end
    return "  " .. label .. " " .. format_range(line_start(node), line_end(node))
  end

  local function walk(node, source, out)
    local descriptor = config.nodes[node:type()]
    if descriptor then
      out[#out + 1] = render(node, source, descriptor)
      if not descriptor.recurse then
        return
      end
    end
    for _, child in ipairs(node:children()) do
      walk(child, source, out)
    end
  end

  return {
    extract = function(source, root)
      local out = {}
      walk(root, source, out)
      if #out == 0 then
        return ""
      end
      return (config.header or "outline") .. ":\n" .. table.concat(out, "\n") .. "\n"
    end,
  }
end
