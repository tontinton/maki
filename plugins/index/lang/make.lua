return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local find_child = U.find_child
  local line_start = U.line_start
  local truncate = U.truncate
  local new_entry = U.new_entry
  local simple_import = U.simple_import
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION

  local VALUE_TRUNCATE = 80
  local INCLUDE_PREFIXES = { "%-include%s+", "sinclude%s+", "include%s+" }

  -- Conditionals hold their body inline (the else branch lives in a nested
  -- directive node), and they are not a scope, so we walk through them and
  -- keep the entries flat, in source order, both branches included.
  local CONDITIONAL_KINDS = { conditional = true, else_directive = true, elsif_directive = true }

  -- Make allows tab indented lines inside a conditional that is not part of a
  -- rule, and the grammar reports those as recipe lines. Only rules are a
  -- recipe scope and we never walk into them, so an assignment shaped recipe
  -- line reached here is really a variable.
  local ASSIGNMENT_PATTERN = "^[%w_.%-]+%s*[:+?!]?="

  local function trimmed(node, source)
    return (compact_ws(get_text(node, source)):gsub("^%s+", ""):gsub("%s+$", ""))
  end

  -- Make grammar nodes swallow the blank lines that follow them; report the
  -- last line that has content instead.
  local function last_content_line(node, source)
    local text = (get_text(node, source):gsub("%s+$", ""))
    local _, newlines = text:gsub("\n", "")
    return line_start(node) + newlines
  end

  local function entry_for(node, source)
    local kind = node:type()
    if kind == "rule" then
      local targets = find_child(node, "targets")
      return targets and new_entry(SECTION.Target, node, trimmed(targets, source) .. ":")
    end
    if kind == "variable_assignment" then
      return new_entry(SECTION.Constant, node, truncate(trimmed(node, source), VALUE_TRUNCATE))
    end
    if kind == "define_directive" then
      local name = node:field("name")[1]
      return name and new_entry(SECTION.Constant, node, "define " .. get_text(name, source))
    end
    if kind == "include_directive" then
      return simple_import(node, source, INCLUDE_PREFIXES, "/")
    end
    if kind == "recipe_line" then
      local text = trimmed(node, source)
      return text:match(ASSIGNMENT_PATTERN) and new_entry(SECTION.Constant, node, truncate(text, VALUE_TRUNCATE))
    end
    return nil
  end

  local function scan(node, source, entries)
    for _, child in ipairs(node:children()) do
      if CONDITIONAL_KINDS[child:type()] then
        scan(child, source, entries)
      else
        local entry = entry_for(child, source)
        if entry then
          entry.line_end = last_content_line(child, source)
          entries[#entries + 1] = entry
        end
      end
    end
  end

  return {
    extract = function(source, root)
      local entries = {}
      scan(root, source, entries)
      return format_skeleton(entries, {}, nil, "/")
    end,
  }
end
