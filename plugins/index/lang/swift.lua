return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local prefixed = U.prefixed
  local extract_body_members = U.extract_body_members
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end

  local function modifiers_text(node, source)
    local mods = find_child(node, "modifiers")
    if not mods then return "" end
    local parts = {}
    for _, child in ipairs(mods:children()) do
      local ckind = child:type()
      if ckind == "visibility_modifier" then
        local t = get_text(child, source)
        parts[#parts + 1] = t:match("^(%S+)") or t
      elseif ckind == "function_modifier" or ckind == "member_modifier"
          or ckind == "mutation_modifier" then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    return table.concat(parts, " ")
  end

  local function params_text(node, source)
    local params = {}
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "parameter" then
        local ext = child:field("external_name")[1]
        local nm = child:field("name")[1]
        local ext_str = ext and (get_text(ext, source) .. " ") or ""
        local name_str = nm and get_text(nm, source) or "_"
        local type_node = child:field("type")[1]
        local type_str = type_node and (": " .. get_text(type_node, source)) or ""
        params[#params + 1] = ext_str .. name_str .. type_str
      end
    end
    return "(" .. table.concat(params, ", ") .. ")"
  end

  local function swift_fn_signature(node, source)
    local mods = modifiers_text(node, source)
    local keyword = "func"
    for _, child in ipairs(node:children()) do
      local t = child:type()
      if t == "func" or t == "init" then keyword = t; break end
    end
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or ""
    local params_node = find_child(node, "function_value_parameters")
    local params = params_node and params_text(params_node, source) or "()"
    local throws_node = find_child(node, "throws")
    local throws_str = throws_node and (" " .. get_text(throws_node, source)) or ""
    local ret_node = node:field("return_type")[1]
    local ret_str = ret_node and (" -> " .. get_text(ret_node, source)) or ""
    local sig = keyword .. " " .. name .. params .. throws_str .. ret_str
    return compact_ws(prefixed(mods, sig))
  end

  local function property_text_str(node, source)
    local mods = modifiers_text(node, source)
    local keyword = "var"
    for _, child in ipairs(node:children()) do
      local t = child:type()
      if t == "let" or t == "var" then keyword = t; break end
    end
    local name_node = node:field("name")[1]
      or find_child(node, "bound_identifier")
    local name = name_node and get_text(name_node, source) or "_"
    local type_node = find_child(node, "type_annotation")
    local type_str = type_node and (" " .. get_text(type_node, source)) or ""
    return compact_ws(prefixed(mods, keyword .. " " .. name .. type_str))
  end

  local function inheritance_text(node, source)
    local inh = find_child(node, "inheritance_specifier")
    if not inh then return "" end
    local parts = {}
    for _, child in ipairs(inh:children()) do
      local ckind = child:type()
      if ckind ~= "," and ckind ~= ":" then
        local t = get_text(child, source):match("^%s*(.-)%s*$")
        if #t > 0 then parts[#parts + 1] = t end
      end
    end
    return #parts > 0 and (": " .. table.concat(parts, ", ")) or ""
  end

  local CLASS_RULES = {
    { kind = "function_declaration",  handler = "method", fn = swift_fn_signature },
    { kind = "init_declaration",      handler = "method", fn = swift_fn_signature },
    { kind = "property_declaration",  handler = "field",  fn = property_text_str, counter = "field" },
  }

  local function extract_class_like(node, source, section, keyword)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local inh = inheritance_text(node, source)
    local text = compact_ws(prefixed(mods, keyword .. " " .. name .. inh))
    local entry = new_entry(section, node, text)
    local body = find_child(node, "class_body")
    if body then
      entry.children = extract_body_members(body, source, CLASS_RULES)
    end
    return entry
  end

  local function extract_enum_body(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local inh = inheritance_text(node, source)
    local text = compact_ws(prefixed(mods, "enum " .. name .. inh))
    local entry = new_entry(SECTION.Type, node, text)
    local body = find_child(node, "enum_class_body")
    if not body then return entry end
    local children = {}
    for _, child in ipairs(body:children()) do
      local ckind = child:type()
      if ckind == "enum_entry" then
        local cases = {}
        for _, cc in ipairs(child:children()) do
          if cc:type() == "simple_identifier" then
            cases[#cases + 1] = "case " .. get_text(cc, source)
          end
        end
        local lr = format_range(line_start(child), line_end(child))
        if #cases > 0 then
          children[#children + 1] = table.concat(cases, ", ") .. " " .. lr
        end
      elseif ckind == "function_declaration" then
        local sig = swift_fn_signature(child, source)
        if sig then
          local lr = format_range(line_start(child), line_end(child))
          children[#children + 1] = sig .. " " .. lr
        end
      end
    end
    entry.children = children
    entry.child_kind = CHILD_BRIEF
    return entry
  end

  local function extract_import(node, source)
    local text = get_text(node, source)
    local cleaned = text:match("^import%s+(.+)") or text
    cleaned = cleaned:match("^%s*(.-)%s*$")
    local last = cleaned:match("%S+$") or cleaned
    local parts = {}
    for seg in last:gmatch("[^%.]+") do parts[#parts + 1] = seg end
    return new_import_entry(node, { parts })
  end

  local function extract_property(node, source)
    for _, child in ipairs(node:children()) do
      if child:type() == "let" then
        local text = property_text_str(node, source)
        return new_entry(SECTION.Constant, node, text)
      end
    end
    return nil
  end

  local function extract_function(node, source)
    local sig = swift_fn_signature(node, source)
    if not sig then return nil end
    return new_entry(SECTION.Function, node, sig)
  end

  local function extract_typealias(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local val_node = node:field("value")[1]
    local val_str = val_node and (" = " .. get_text(val_node, source)) or ""
    return new_entry(SECTION.Type, node, compact_ws(prefixed(mods, "typealias " .. name .. val_str)))
  end

  local PROTOCOL_RULES = {
    { kind = "protocol_function_declaration",  handler = "method", fn = swift_fn_signature },
    { kind = "protocol_property_declaration",  handler = "field",  fn = function(n, source)
        local nn = find_child(n, "simple_identifier") or n:field("name")[1]
        local name = nn and get_text(nn, source) or "_"
        local type_node = find_child(n, "type_annotation")
        local type_str = type_node and (" " .. get_text(type_node, source)) or ""
        return compact_ws("var " .. name .. type_str)
      end, counter = "field" },
  }

  local function extract_protocol(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local inh = inheritance_text(node, source)
    local text = compact_ws(prefixed(mods, "protocol " .. name .. inh))
    local entry = new_entry(SECTION.Trait, node, text)
    local body = find_child(node, "protocol_body")
    if body then
      entry.children = extract_body_members(body, source, PROTOCOL_RULES)
    end
    return entry
  end

  local function extract_class_declaration(node, source)
    local kind_node = node:field("declaration_kind")[1]
    local kind_str = kind_node and get_text(kind_node, source) or ""
    if kind_str == "class" or kind_str == "actor" then
      return extract_class_like(node, source, SECTION.Class, kind_str)
    elseif kind_str == "struct" then
      return extract_class_like(node, source, SECTION.Type, "struct")
    elseif kind_str == "enum" then
      return extract_enum_body(node, source)
    elseif kind_str == "extension" then
      return extract_class_like(node, source, SECTION.Impl, "extension")
    end
    return nil
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, source)
      local kind = node:type()
      if kind ~= "comment" and kind ~= "multiline_comment" then return false end
      local text = get_text(node, source)
      return text:sub(1, 3) == "///" or text:sub(1, 3) == "/**"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "import_declaration" then
        return { extract_import(node, source) }
      elseif kind == "class_declaration" then
        local e = extract_class_declaration(node, source)
        return e and { e } or {}
      elseif kind == "function_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}
      elseif kind == "property_declaration" then
        local e = extract_property(node, source)
        return e and { e } or {}
      elseif kind == "typealias_declaration" then
        local e = extract_typealias(node, source)
        return e and { e } or {}
      elseif kind == "protocol_declaration" then
        local e = extract_protocol(node, source)
        return e and { e } or {}
      end
      return {}
    end,
  }
end
