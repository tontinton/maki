return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local simple_import = U.simple_import
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local prefixed = U.prefixed
  local extract_enum_variants = U.extract_enum_variants
  local extract_body_members = U.extract_body_members
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end

  local MODIFIER_KEYWORDS = {
    public = true, private = true, protected = true, internal = true,
    static = true, abstract = true, sealed = true, override = true,
    virtual = true, async = true, readonly = true, extern = true,
    partial = true, new = true, unsafe = true, volatile = true,
  }

  local function modifiers_text_free(node, source)
    local parts = {}
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "modifier" then
        local t = get_text(child, source)
        if MODIFIER_KEYWORDS[t] then parts[#parts + 1] = t end
      elseif ckind == "attribute_list" then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    return table.concat(parts, " ")
  end

  local function base_list_text(node, source)
    local bl = find_child(node, "base_list")
    if not bl then return "" end
    local t = get_text(bl, source):match("^%s*(.-)%s*$")
    t = t:gsub("^:", ""):match("^%s*(.-)%s*$")
    return " : " .. t
  end

  local function method_signature_free(node, source)
    local mods = modifiers_text_free(node, source)
    local ret = node:field("returns")[1] or node:field("type")[1]
    local ret_str = ret and (get_text(ret, source) .. " ") or ""
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = find_child(node, "parameter_list")
    local params = params_node and get_text(params_node, source) or "()"
    return compact_ws(prefixed(mods, ret_str .. name .. params))
  end

  local function field_text_free(node, source)
    local mods = modifiers_text_free(node, source)
    local decl = find_child(node, "variable_declaration")
    if not decl then return compact_ws(prefixed(mods, get_text(node, source))) end
    local type_node = decl:field("type")[1]
    local type_str = type_node and get_text(type_node, source) or "_"
    local names = {}
    for _, child in ipairs(decl:children()) do
      if child:type() == "variable_declarator" then
        local nn = child:field("name")[1]
        if nn then names[#names + 1] = get_text(nn, source) end
      end
    end
    local name_str = #names > 0 and table.concat(names, ", ") or "_"
    return compact_ws(prefixed(mods, type_str .. " " .. name_str))
  end

  local function property_text_free(node, source)
    local mods = modifiers_text_free(node, source)
    local type_node = node:field("type")[1]
    local type_str = type_node and (get_text(type_node, source) .. " ") or ""
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or "_"
    return compact_ws(prefixed(mods, type_str .. name))
  end

  local CLASS_RULES = {
    { kind = "method_declaration",      handler = "method", fn = method_signature_free },
    { kind = "constructor_declaration", handler = "method", fn = method_signature_free },
    { kind = "field_declaration",       handler = "field",  fn = field_text_free,     counter = "field" },
    { kind = "property_declaration",    handler = "field",  fn = property_text_free,  counter = "field" },
  }

  local INTERFACE_RULES = {
    { kind = "method_declaration",   handler = "method", fn = method_signature_free },
    { kind = "property_declaration", handler = "field",  fn = property_text_free,   counter = "field" },
  }

  local function extract_class_like(node, source, section, keyword)
    local mods = modifiers_text_free(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local bases = base_list_text(node, source)
    local text = compact_ws(prefixed(mods, keyword .. " " .. name .. " " .. bases))
    local entry = new_entry(section, node, text)
    local body = find_child(node, "declaration_list")
    if body then
      entry.children = extract_body_members(body, source, CLASS_RULES)
    end
    return entry
  end

  local function extract_interface(node, source)
    local mods = modifiers_text_free(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local bases = base_list_text(node, source)
    local text = compact_ws(prefixed(mods, "interface " .. name .. bases))
    local entry = new_entry(SECTION.Trait, node, text)
    local body = find_child(node, "declaration_list")
    if body then
      entry.children = extract_body_members(body, source, INTERFACE_RULES)
    end
    return entry
  end

  local function extract_enum(node, source)
    local mods = modifiers_text_free(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local text = compact_ws(prefixed(mods, "enum " .. name))
    local entry = new_entry(SECTION.Type, node, text)
    local body = find_child(node, "enum_member_declaration_list")
    if body then
      entry.children = extract_enum_variants(body, source, "enum_member_declaration")
      entry.child_kind = CHILD_BRIEF
    end
    return entry
  end

  local function extract_record(node, source)
    local mods = modifiers_text_free(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = find_child(node, "parameter_list")
    local params = params_node and get_text(params_node, source) or ""
    local bases = base_list_text(node, source)
    local text = compact_ws(prefixed(mods, "record " .. name .. params .. bases))
    return new_entry(SECTION.Type, node, text)
  end

  local function extract_namespace(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    return new_entry(SECTION.Module, node, get_text(name_node, source))
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, source)
      local kind = node:type()
      if kind == "single_line_doc_comment" then return true end
      if kind == "comment" then
        return get_text(node, source):sub(1, 3) == "///"
      end
      return false
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "using_directive" then
        return { simple_import(node, source, {"using%s+"}, ".") }
      elseif kind == "namespace_declaration" or kind == "file_scoped_namespace_declaration" then
        local e = extract_namespace(node, source)
        return e and { e } or {}
      elseif kind == "class_declaration" then
        local e = extract_class_like(node, source, SECTION.Class, "class")
        return e and { e } or {}
      elseif kind == "struct_declaration" then
        local e = extract_class_like(node, source, SECTION.Type, "struct")
        return e and { e } or {}
      elseif kind == "interface_declaration" then
        local e = extract_interface(node, source)
        return e and { e } or {}
      elseif kind == "enum_declaration" then
        local e = extract_enum(node, source)
        return e and { e } or {}
      elseif kind == "record_declaration" then
        local e = extract_record(node, source)
        return e and { e } or {}
      end
      return {}
    end,
  }
end
