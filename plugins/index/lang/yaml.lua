-- YAML is data, not code: there are no functions or types to extract.
-- The useful skeleton is the top-level mapping keys (config sections, service
-- names, etc.) with one level of nested mapping keys as their children, so a
-- glance at the index shows the document's shape. Sequence values and scalars
-- are not indexed on their own — they only matter as the body of the key that
-- owns them. Sequences of mappings are unwrapped one level so a list of objects
-- still contributes its keys.
return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local new_entry = U.new_entry
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION

  local pair_entry, for_each_pair

  local function key_text(pair, source)
    local key_node = pair:field("key")[1]
    if not key_node then
      return nil
    end
    local raw = get_text(key_node, source)
    local cleaned = compact_ws(raw):gsub("^%s+", ""):gsub("%s+$", "")
    cleaned = cleaned:gsub('^"(.-)"$', "%1"):gsub("^'(.-)'$", "%1")
    if cleaned == "" then
      return nil
    end
    return cleaned
  end

  local SEQUENCE_KINDS = {
    block_sequence = true,
    block_sequence_item = true,
    flow_sequence = true,
  }

  local function for_each_pair_fn(node, source, callback)
    if not node then
      return
    end
    local kind = node:type()
    if kind == "block_node" or kind == "flow_node" or SEQUENCE_KINDS[kind] then
      for _, child in ipairs(node:children()) do
        for_each_pair(child, source, callback)
      end
      return
    end
    if kind == "block_mapping" or kind == "flow_mapping" then
      for _, child in ipairs(node:children()) do
        local ck = child:type()
        if ck == "block_mapping_pair" or ck == "flow_pair" then
          callback(child)
        end
      end
    end
  end
  for_each_pair = for_each_pair_fn

  local function pair_entry_fn(pair, source, recurse)
    local key = key_text(pair, source)
    if not key then
      return nil
    end
    local entry = new_entry(SECTION.Constant, pair, key)
    if recurse then
      local children = {}
      for_each_pair(pair:field("value")[1], source, function(child)
        local e = pair_entry(child, source, false)
        if e then
          children[#children + 1] = e
        end
      end)
      entry.children = children
    end
    return entry
  end
  pair_entry = pair_entry_fn

  return {
    extract = function(source, root)
      local entries = {}
      for _, child in ipairs(root:children()) do
        if child:type() == "document" then
          for _, doc_child in ipairs(child:children()) do
            local kind = doc_child:type()
            if kind == "block_node" or kind == "flow_node" then
              for _, nested in ipairs(doc_child:children()) do
                for_each_pair(nested, source, function(pair)
                  local e = pair_entry(pair, source, true)
                  if e then
                    entries[#entries + 1] = e
                  end
                end)
              end
            else
              for_each_pair(doc_child, source, function(pair)
                local e = pair_entry(pair, source, true)
                if e then
                  entries[#entries + 1] = e
                end
              end)
            end
          end
        else
          for_each_pair(child, source, function(pair)
            local e = pair_entry(pair, source, true)
            if e then
              entries[#entries + 1] = e
            end
          end)
        end
      end
      return format_skeleton(entries, {}, nil, ".")
    end,
  }
end
