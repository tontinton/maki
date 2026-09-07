return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local line_start = U.line_start
  local new_entry = U.new_entry
  local prefixed = U.prefixed
  local new_import_entry = U.new_import_entry
  local extract_fields_truncated = U.extract_fields_truncated
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF

  local MEMBER_KINDS = { "struct_field_declaration", "interface_method_definition" }
  local PARSE_ERROR = "cannot index .v source as V: parse errors; use read instead"

  local function field_text(node, field, source)
    local value = node:field(field)[1]
    return value and get_text(value, source) or nil
  end

  local function visibility(node)
    return find_child(node, "visibility_modifiers") and "pub" or ""
  end

  local function extract_import(node, source)
    local spec = find_child(node, "import_spec")
    local path_node = spec and find_child(spec, "import_path")
    if not path_node then
      return nil
    end
    local path = {}
    for part in get_text(path_node, source):gmatch("[^.]+") do
      path[#path + 1] = part
    end
    return new_import_entry(node, { path })
  end

  -- `const (...)` and `__global (...)` hold many definitions under one node. The
  -- first entry borrows the group's start line so the opening line, and any
  -- attribute sitting above it, stay inside the reported range.
  local function extract_specs(node, source, kind, prefix)
    local vis = visibility(node)
    local entries = {}
    for _, child in ipairs(node:children()) do
      if child:type() == kind then
        local name = field_text(child, "name", source)
        if name then
          local type_node = find_child(child, "plain_type")
          local suffix = type_node and (" " .. get_text(type_node, source)) or ""
          entries[#entries + 1] = new_entry(SECTION.Constant, child, prefixed(vis, prefix .. name .. suffix))
        end
      end
    end
    if entries[1] then
      entries[1].line_start = line_start(node)
    end
    return entries
  end

  -- Structs, unions and interfaces all carry `struct_field_declaration`, and an
  -- interface only adds methods on top, so one formatter serves all three.
  local function format_member(node, source)
    local name = field_text(node, "name", source)
    local member_type = field_text(node, "type", source)
    local signature = field_text(node, "signature", source)
    local member
    if signature then
      member = name and (name .. (field_text(node, "generic_parameters", source) or "") .. signature)
    elseif name and member_type then
      member = name .. " " .. member_type
    else
      local embedded = find_child(node, "embedded_definition")
      member = member_type or name or (embedded and get_text(embedded, source))
    end
    return member and compact_ws(member)
  end

  local function extract_container(node, source, keyword)
    local name = field_text(node, "name", source)
    if not name then
      return nil
    end
    local generics = field_text(node, "generic_parameters", source) or ""
    local entry = new_entry(SECTION.Type, node, prefixed(visibility(node), keyword .. " " .. name .. generics))
    entry.children = extract_fields_truncated(node, source, MEMBER_KINDS, format_member)
    return entry
  end

  local function extract_enum(node, source)
    local name = field_text(node, "name", source)
    if not name then
      return nil
    end
    local entry = new_entry(SECTION.Type, node, prefixed(visibility(node), "enum " .. name))
    entry.children = extract_fields_truncated(node, source, "enum_field_definition", function(member, src)
      return field_text(member, "name", src)
    end)
    entry.child_kind = CHILD_BRIEF
    return entry
  end

  local function extract_type(node, source)
    local name = field_text(node, "name", source)
    local aliased = field_text(node, "type", source)
    if not name or not aliased then
      return nil
    end
    local generics = field_text(node, "generic_parameters", source) or ""
    return new_entry(
      SECTION.Type,
      node,
      prefixed(visibility(node), "type " .. name .. generics .. " = " .. compact_ws(aliased))
    )
  end

  local function extract_function(node, source)
    local name = field_text(node, "name", source)
    if not name then
      return nil
    end
    local receiver = field_text(node, "receiver", source)
    local static_receiver = field_text(node, "static_receiver", source)
    local generics = field_text(node, "generic_parameters", source) or ""
    local signature = name .. generics .. (field_text(node, "signature", source) or "()")
    if static_receiver then
      signature = static_receiver .. "." .. signature
    elseif receiver then
      signature = receiver .. " " .. signature
    end
    local section = (receiver or static_receiver) and SECTION.Impl or SECTION.Function
    return new_entry(section, node, prefixed(visibility(node), compact_ws(signature)))
  end

  return {
    import_separator = ".",

    -- `.v` is shared with verilog and coq, and a half-written V file looks just
    -- as foreign. A skeleton built from a broken tree is worse than none, so bail.
    validate_root = function(root)
      if root:has_error() then
        return PARSE_ERROR
      end
    end,

    is_doc_comment = function(node, _source)
      return node:type() == "line_comment" or node:type() == "block_comment"
    end,

    is_test_node = function(node, source, _attrs)
      if node:type() ~= "function_declaration" or node:field("receiver")[1] then
        return false
      end
      local name = field_text(node, "name", source)
      return name ~= nil and name:sub(1, 5) == "test_"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "import_list" then
        local entries = {}
        for _, child in ipairs(node:named_children()) do
          local entry = extract_import(child, source)
          if entry then
            entries[#entries + 1] = entry
          end
        end
        return entries
      elseif kind == "import_declaration" then
        local entry = extract_import(node, source)
        return entry and { entry } or {}
      elseif kind == "const_declaration" then
        return extract_specs(node, source, "const_definition", "const ")
      elseif kind == "global_var_declaration" then
        return extract_specs(node, source, "global_var_definition", "global ")
      elseif kind == "struct_declaration" then
        local entry = extract_container(node, source, find_child(node, "union") and "union" or "struct")
        return entry and { entry } or {}
      elseif kind == "interface_declaration" then
        local entry = extract_container(node, source, "interface")
        return entry and { entry } or {}
      elseif kind == "enum_declaration" then
        local entry = extract_enum(node, source)
        return entry and { entry } or {}
      elseif kind == "type_declaration" then
        local entry = extract_type(node, source)
        return entry and { entry } or {}
      elseif kind == "function_declaration" or kind == "static_method_declaration" then
        local entry = extract_function(node, source)
        return entry and { entry } or {}
      end
      return {}
    end,
  }
end
