-- CSS has no functions or types: the shape of a stylesheet is its selectors
-- and at-rules. Declarations are the body of the rule that owns them and are
-- never indexed on their own.
return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local find_child = U.find_child
  local new_entry = U.new_entry
  local simple_import = U.simple_import
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION

  local LABEL_TRUNCATE = 80
  local IMPORT_PREFIXES = { "@import%s+" }

  -- At-rules, labeled with their prelude (`@media screen and (min-width: 0)`).
  local STATEMENT_KINDS = {
    at_rule = true,
    keyframes_statement = true,
    media_statement = true,
    scope_statement = true,
    supports_statement = true,
  }

  local function label(text)
    return truncate((compact_ws(text):gsub("^%s+", ""):gsub("%s+$", "")), LABEL_TRUNCATE)
  end

  local function rule_entry(node, source)
    local kind = node:type()
    if kind == "rule_set" then
      return new_entry(SECTION.Rule, node, label(get_text(find_child(node, "selectors") or node, source)))
    end
    if STATEMENT_KINDS[kind] then
      -- A prelude cannot contain `{`, so the first brace is where the block
      -- starts and everything before it is the label.
      return new_entry(SECTION.Rule, node, label(get_text(node, source):match("^[^{]*")))
    end
    return nil
  end

  -- Rules nested one level deep (inside `@media`, or CSS nesting) are worth a
  -- line of their own; anything deeper is detail.
  local function top_entry(node, source)
    local entry = rule_entry(node, source)
    if not entry then
      return nil
    end
    local block = find_child(node, "block")
    for _, child in ipairs(block and block:children() or {}) do
      local nested = rule_entry(child, source)
      if nested then
        entry.children[#entry.children + 1] = nested
      end
    end
    return entry
  end

  return {
    extract = function(source, root)
      local entries = {}
      for _, child in ipairs(root:children()) do
        if child:type() == "import_statement" then
          entries[#entries + 1] = simple_import(child, source, IMPORT_PREFIXES, "/")
        else
          local entry = top_entry(child, source)
          if entry then
            entries[#entries + 1] = entry
          end
        end
      end
      return format_skeleton(entries, {}, nil, "/")
    end,
  }
end
