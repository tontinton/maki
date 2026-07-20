-- TOML indexer.
--
-- TOML files are nested tables of key/value pairs rather than functions or
-- classes, so this uses the custom `extract` style (like lang/nix.lua)
-- instead of the shared extract_nodes/default_extract loop: each top-level
-- `[table]` / `[[table_array_element]]` becomes a Constant entry labeled
-- with its dotted path, with its direct `pair` children flattened into lines.
-- The first FIELD_TRUNCATE_THRESHOLD pairs render as `key = value` (value
-- truncated), and the rest render as `key` only, so every key is visible
-- without exploding the skeleton.
-- Top-level pairs (before any table header) become their own Constant
-- entries, mirroring how lang/nix.lua renders top-level bindings.
--
-- The grammar (tree-sitter-toml-ng) has no doc-comment convention and no
-- imports, and declares no named fields at all -- every lookup below walks
-- `node:children()` and matches on `:type()`.

return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION
  local FIELD_TRUNCATE_THRESHOLD = U.FIELD_TRUNCATE_THRESHOLD

  local VALUE_TRUNCATE = 60

  local KEY_KINDS = {
    bare_key = true,
    dotted_key = true,
    quoted_key = true,
  }

  local VALUE_KINDS = {
    string = true,
    integer = true,
    float = true,
    boolean = true,
    offset_date_time = true,
    local_date_time = true,
    local_date = true,
    local_time = true,
    array = true,
    inline_table = true,
  }

  -- A `pair` node's children are just its key node (bare_key / dotted_key /
  -- quoted_key) followed by exactly one value node -- no fields to key off
  -- of, so we take the first key-shaped child and the first value-shaped
  -- child that comes after it.
  local function pair_key_value(node)
    local key, value
    for _, child in ipairs(node:children()) do
      local kind = child:type()
      if not key and KEY_KINDS[kind] then
        key = child
      elseif key and not value and VALUE_KINDS[kind] then
        value = child
      end
    end
    return key, value
  end

  -- Inline tables/arrays and multiline strings are rendered as a single
  -- truncated line rather than recursed into -- only `[table]` /
  -- `[[table_array_element]]` headers create nesting in the skeleton.
  local function format_pair(node, source, include_value)
    if include_value == nil then
      include_value = true
    end
    local key, value = pair_key_value(node)
    if not key then
      return nil
    end
    local key_text = get_text(key, source)
    if include_value and value then
      return key_text .. " = " .. truncate(compact_ws(get_text(value, source)), VALUE_TRUNCATE)
    end
    return key_text
  end

  local function table_header(node)
    for _, child in ipairs(node:children()) do
      if KEY_KINDS[child:type()] then
        return child
      end
    end
    return nil
  end

  local function table_pairs(node)
    local pairs_list = {}
    for _, child in ipairs(node:children()) do
      if child:type() == "pair" then
        pairs_list[#pairs_list + 1] = child
      end
    end
    return pairs_list
  end

  local function build_table_entry(node, source, is_array)
    local header = table_header(node)
    local path = header and get_text(header, source) or "?"
    local label = is_array and ("[[" .. path .. "]]") or ("[" .. path .. "]")
    local entry = new_entry(SECTION.Constant, node, label)

    for i, p in ipairs(table_pairs(node)) do
      local text = format_pair(p, source, i <= FIELD_TRUNCATE_THRESHOLD)
      if text then
        entry.children[#entry.children + 1] = text
      end
    end

    return entry
  end

  return {
    extract = function(source, root)
      local entries = {}
      for _, child in ipairs(root:children()) do
        local kind = child:type()
        if kind == "pair" then
          local text = format_pair(child, source)
          if text then
            entries[#entries + 1] = new_entry(SECTION.Constant, child, text)
          end
        elseif kind == "table" then
          entries[#entries + 1] = build_table_entry(child, source, false)
        elseif kind == "table_array_element" then
          entries[#entries + 1] = build_table_entry(child, source, true)
        end
      end
      return format_skeleton(entries, {}, nil, "")
    end,
  }
end
