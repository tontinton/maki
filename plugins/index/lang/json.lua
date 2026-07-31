-- JSON is data, not code, so like lang/yaml.lua the skeleton is the top-level
-- object keys with one level of nested keys as children. Arrays are unwrapped
-- so a list of objects still contributes its keys.
return function(U)
  local get_text = U.get_text
  local new_entry = U.new_entry
  local truncated_msg = U.truncated_msg
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION
  local FIELD_TRUNCATE_THRESHOLD = U.FIELD_TRUNCATE_THRESHOLD

  -- `object` and `array` are matched by name because the grammar's `_value`
  -- supertype is hidden and never appears in the tree.
  local function for_each_pair(node, callback)
    if not node then
      return
    end
    local kind = node:type()
    if kind == "object" then
      for _, child in ipairs(node:children()) do
        if child:type() == "pair" then
          callback(child)
        end
      end
    elseif kind == "array" then
      for _, child in ipairs(node:children()) do
        for_each_pair(child, callback)
      end
    end
  end

  local function key_entry(pair, source)
    local key = pair:field("key")[1]
    return key and new_entry(SECTION.Constant, pair, get_text(key, source))
  end

  local function top_entry(pair, source)
    local entry = key_entry(pair, source)
    if not entry then
      return nil
    end
    -- Lock files list thousands of near-identical keys under one entry
    -- (npm's "packages"), so only the first few say anything about the shape.
    local total = 0
    for_each_pair(pair:field("value")[1], function(child)
      local nested = key_entry(child, source)
      if nested then
        total = total + 1
        if total <= FIELD_TRUNCATE_THRESHOLD then
          entry.children[#entry.children + 1] = nested
        end
      end
    end)
    if total > FIELD_TRUNCATE_THRESHOLD then
      entry.children[#entry.children + 1] = truncated_msg(total)
    end
    return entry
  end

  return {
    extract = function(source, root)
      local entries = {}
      for _, child in ipairs(root:children()) do
        for_each_pair(child, function(pair)
          local entry = top_entry(pair, source)
          if entry then
            entries[#entries + 1] = entry
          end
        end)
      end
      return format_skeleton(entries, {}, nil, "")
    end,
  }
end
