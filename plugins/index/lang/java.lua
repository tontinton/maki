return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local simple_import = U.simple_import
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local extract_enum_variants = U.extract_enum_variants
  local extract_body_members = U.extract_body_members

  local function modifiers_text(node, source)
    local mods = find_child(node, "modifiers")
    if not mods then return "" end
    local parts = {}
    for _, child in ipairs(mods:children()) do
      local ck = child:type()
      local text = get_text(child, source)
      if ck == "marker_annotation" or ck == "annotation" then
        parts[#parts + 1] = text
      elseif text:match("^public$") or text:match("^private$") or text:match("^protected$")
          or text:match("^static$") or text:match("^final$") or text:match("^abstract$")
          or text:match("^default$") or text:match("^synchronized$") then
        parts[#parts + 1] = text
      end
    end
    return table.concat(parts, " ")
  end

  local function tparams_text(node, source)
    local tp = find_child(node, "type_parameters")
    return tp and get_text(tp, source) or ""
  end

  local function method_sig(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local ret_node = node:field("type")[1]
    local ret = ret_node and (get_text(ret_node, source) .. " ") or ""
    local params_node = node:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "()"
    return compact_ws(mods ~= "" and (mods .. " " .. ret .. name .. params) or (ret .. name .. params))
  end

  local function field_text(node, source)
    local mods = modifiers_text(node, source)
    local type_node = node:field("type")[1]
    local type_str = type_node and get_text(type_node, source) or ""
    local decl = find_child(node, "variable_declarator")
    local fname = decl and decl:field("name")[1]
    local fname_str = fname and get_text(fname, source) or "_"
    return compact_ws(mods ~= "" and (mods .. " " .. type_str .. " " .. fname_str) or (type_str .. " " .. fname_str))
  end

  local CLASS_RULES = {
    { kind = "method_declaration",      handler = "method", fn = method_sig },
    { kind = "constructor_declaration", handler = "method", fn = method_sig },
    { kind = "field_declaration",       handler = "field",  fn = field_text, counter = "field" },
  }

  local INTERFACE_RULES = {
    { kind = "method_declaration",   handler = "method", fn = method_sig },
    { kind = "constant_declaration", handler = "field",  fn = field_text, counter = "field" },
  }

  local function type_list_text(parent, source)
    local tl = find_child(parent, "type_list")
    if tl then return get_text(tl, source) end
    local raw = get_text(parent, source)
    return raw:gsub("^extends%s*", ""):gsub("^implements%s*", "")
  end

  local function implements_clause(node, source)
    local ifaces = node:field("interfaces")[1]
    if not ifaces then return "" end
    return " implements " .. type_list_text(ifaces, source)
  end

  local function extract_class(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local super_node = node:field("superclass")[1]
    local super = ""
    if super_node then
      local type_id = find_child(super_node, "type_identifier")
      super = " extends " .. get_text(type_id or super_node, source)
    end
    local interfaces = implements_clause(node, source)
    local label = compact_ws(mods ~= "" and (mods .. " class " .. name .. tparams .. super .. interfaces)
                             or ("class " .. name .. tparams .. super .. interfaces))
    local entry = new_entry(SECTION.Class, node, label)
    local body = find_child(node, "class_body")
    if body then entry.children = extract_body_members(body, source, CLASS_RULES) end
    return entry
  end

  local function extract_interface(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local ext_node = find_child(node, "extends_interfaces")
    local ext_str = ext_node and (" extends " .. type_list_text(ext_node, source)) or ""
    local label = compact_ws(mods ~= "" and (mods .. " interface " .. name .. tparams .. ext_str)
                             or ("interface " .. name .. tparams .. ext_str))
    local entry = new_entry(SECTION.Trait, node, label)
    local body = find_child(node, "interface_body")
    if body then entry.children = extract_body_members(body, source, INTERFACE_RULES) end
    return entry
  end

  local function extract_enum(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local interfaces = implements_clause(node, source)
    local label = compact_ws(mods ~= "" and (mods .. " enum " .. name .. tparams .. interfaces)
                             or ("enum " .. name .. tparams .. interfaces))
    local entry = new_entry(SECTION.Type, node, label)
    local body = find_child(node, "enum_body")
    if body then
      entry.children = extract_enum_variants(body, source, "enum_constant")
      entry.child_kind = CHILD_BRIEF
    end
    return entry
  end

  local function extract_record(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local params_node = find_child(node, "formal_parameters")
    local params = params_node and get_text(params_node, source) or ""
    local interfaces = implements_clause(node, source)
    local label = compact_ws(mods ~= "" and (mods .. " record " .. name .. tparams .. params .. interfaces)
                             or ("record " .. name .. tparams .. params .. interfaces))
    return new_entry(SECTION.Class, node, label)
  end

  local function extract_annotation_type(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local label = compact_ws(mods ~= "" and (mods .. " @interface " .. name) or ("@interface " .. name))
    return new_entry(SECTION.Type, node, label)
  end

  local function extract_package(node, source)
    local text = get_text(node, source)
    local cleaned = text:match("^package%s+(.-)%s*;?%s*$") or text
    return new_entry(SECTION.Module, node, cleaned)
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, source)
      if node:type() ~= "block_comment" then return false end
      return get_text(node, source):sub(1, 3) == "/**"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "import_declaration" then
        return { simple_import(node, source, { "import%s+" }, ".") }
      elseif kind == "package_declaration" then
        return { extract_package(node, source) }
      elseif kind == "class_declaration" then
        local e = extract_class(node, source)
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
      elseif kind == "annotation_type_declaration" then
        local e = extract_annotation_type(node, source)
        return e and { e } or {}
      end
      return {}
    end,
  }
end
