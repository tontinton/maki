return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local prefixed = U.prefixed
  local extract_enum_variants = U.extract_enum_variants
  local extract_body_members = U.extract_body_members

  local function modifiers(node, source)
    local parts = {}
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "visibility_modifier" or ckind == "static_modifier"
        or ckind == "abstract_modifier" or ckind == "final_modifier"
        or ckind == "readonly_modifier" then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    return table.concat(parts, " ")
  end

  local function use_clause_path(clause, source)
    for _, child in ipairs(clause:children()) do
      local ckind = child:type()
      if ckind == "qualified_name" or ckind == "name" then
        local text = get_text(child, source)
        local parts = {}
        for seg in text:gmatch("[^\\]+") do parts[#parts + 1] = seg end
        return parts
      end
    end
    return nil
  end

  local function extract_use(node, source)
    local paths = {}
    local prefix_parts = {}
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "namespace_use_clause" then
        local p = use_clause_path(child, source)
        if p then paths[#paths + 1] = p end
      elseif ckind == "namespace_name" then
        local text = get_text(child, source)
        for seg in text:gmatch("[^\\]+") do prefix_parts[#prefix_parts + 1] = seg end
      elseif ckind == "namespace_use_group" then
        for _, clause in ipairs(child:children()) do
          if clause:type() == "namespace_use_clause" then
            local p = use_clause_path(clause, source)
            if p then
              local full = {}
              for _, s in ipairs(prefix_parts) do full[#full + 1] = s end
              for _, s in ipairs(p) do full[#full + 1] = s end
              paths[#paths + 1] = full
            end
          end
        end
      end
    end
    if #paths == 0 and #prefix_parts > 0 then
      paths[1] = prefix_parts
    end
    return new_import_entry(node, paths)
  end

  local function params_text(node, source)
    local params_node = find_child(node, "formal_parameters")
    return params_node and get_text(params_node, source) or "()"
  end

  local function return_type_text(node, source)
    local ret = node:field("return_type")[1]
    return ret and (": " .. get_text(ret, source)) or ""
  end

  local function method_sig(node, source)
    local mods = modifiers(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params = params_text(node, source)
    local ret = return_type_text(node, source)
    return compact_ws(prefixed(mods, "function " .. name .. params .. ret))
  end

  local function property_text(node, source)
    local mods = modifiers(node, source)
    local type_node = node:field("type")[1]
    local type_str = type_node and (get_text(type_node, source) .. " ") or ""
    local el = find_child(node, "property_element")
    local name = "_"
    if el then
      local nn = el:field("name")[1]
      if nn then name = get_text(nn, source) end
    end
    return compact_ws(prefixed(mods, type_str .. name))
  end

  local CLASS_RULES = {
    { kind = "method_declaration",   handler = "method", fn = method_sig },
    { kind = "property_declaration", handler = "field",  fn = property_text, counter = "field" },
  }

  local function extract_class(node, source)
    local mods = modifiers(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local base = find_child(node, "base_clause")
    local base_str = ""
    if base then
      for _, ch in ipairs(base:children()) do
        if ch:type() == "qualified_name" or ch:type() == "name" then
          base_str = " extends " .. get_text(ch, source)
          break
        end
      end
    end
    local iface = find_child(node, "class_interface_clause")
    local iface_str = ""
    if iface then
      local iface_names = {}
      for _, ch in ipairs(iface:children()) do
        if ch:type() == "qualified_name" or ch:type() == "name" then
          iface_names[#iface_names + 1] = get_text(ch, source)
        end
      end
      if #iface_names > 0 then
        iface_str = " implements " .. table.concat(iface_names, ", ")
      end
    end
    local text = compact_ws(prefixed(mods, name .. base_str .. iface_str))
    local entry = new_entry(SECTION.Class, node, text)
    local body = find_child(node, "declaration_list")
    if body then
      entry.children = extract_body_members(body, source, CLASS_RULES)
    end
    return entry
  end

  local function extract_interface(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local base = find_child(node, "base_clause")
    local base_str = ""
    if base then
      for _, ch in ipairs(base:children()) do
        if ch:type() == "qualified_name" or ch:type() == "name" then
          base_str = " extends " .. get_text(ch, source)
          break
        end
      end
    end
    local entry = new_entry(SECTION.Trait, node, name .. base_str)
    local body = find_child(node, "declaration_list")
    if body then
      entry.children = extract_body_members(body, source, CLASS_RULES)
    end
    return entry
  end

  local function extract_trait(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local entry = new_entry(SECTION.Trait, node, name)
    local body = find_child(node, "declaration_list")
    if body then
      entry.children = extract_body_members(body, source, CLASS_RULES)
    end
    return entry
  end

  local function extract_function(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params = params_text(node, source)
    local ret = return_type_text(node, source)
    return new_entry(SECTION.Function, node, compact_ws("function " .. name .. params .. ret))
  end

  local function extract_const(node, source)
    local mods = modifiers(node, source)
    local names = {}
    for _, child in ipairs(node:children()) do
      if child:type() == "const_element" then
        local nn = find_child(child, "name")
        if nn then names[#names + 1] = get_text(nn, source) end
      end
    end
    if #names == 0 then return nil end
    local text = compact_ws(prefixed(mods, table.concat(names, ", ")))
    return new_entry(SECTION.Constant, node, text)
  end

  local function extract_enum(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local backing = find_child(node, "primitive_type")
    local backing_str = backing and (": " .. get_text(backing, source)) or ""
    local iface = find_child(node, "class_interface_clause")
    local iface_str = ""
    if iface then
      local iface_names = {}
      for _, ch in ipairs(iface:children()) do
        if ch:type() == "qualified_name" or ch:type() == "name" then
          iface_names[#iface_names + 1] = get_text(ch, source)
        end
      end
      if #iface_names > 0 then
        iface_str = " implements " .. table.concat(iface_names, ", ")
      end
    end
    local text = compact_ws("enum " .. name .. backing_str .. iface_str)
    local entry = new_entry(SECTION.Type, node, text)
    local body = find_child(node, "enum_declaration_list")
    if body then
      entry.children = extract_enum_variants(body, source, "enum_case")
      entry.child_kind = CHILD_BRIEF
    end
    return entry
  end

  local function extract_namespace(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    return new_entry(SECTION.Module, node, get_text(name_node, source))
  end

  return {
    import_separator = "\\",

    is_doc_comment = function(node, source)
      if node:type() ~= "comment" then return false end
      return get_text(node, source):sub(1, 3) == "/**"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "namespace_use_declaration" then
        return { extract_use(node, source) }
      elseif kind == "namespace_definition" then
        local e = extract_namespace(node, source)
        return e and { e } or {}
      elseif kind == "class_declaration" then
        local e = extract_class(node, source)
        return e and { e } or {}
      elseif kind == "interface_declaration" then
        local e = extract_interface(node, source)
        return e and { e } or {}
      elseif kind == "trait_declaration" then
        local e = extract_trait(node, source)
        return e and { e } or {}
      elseif kind == "function_definition" then
        local e = extract_function(node, source)
        return e and { e } or {}
      elseif kind == "const_declaration" then
        local e = extract_const(node, source)
        return e and { e } or {}
      elseif kind == "enum_declaration" then
        local e = extract_enum(node, source)
        return e and { e } or {}
      end
      return {}
    end,
  }
end
