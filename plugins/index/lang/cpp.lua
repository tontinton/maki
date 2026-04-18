return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local extract_enum_variants = U.extract_enum_variants
  local extract_body_members = U.extract_body_members
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF

  local extract_nodes

  local function declarator_name(node, source)
    local kind = node:type()
    if kind == "identifier" or kind == "field_identifier" or kind == "type_identifier"
      or kind == "destructor_name" or kind == "operator_name" or kind == "qualified_identifier"
    then
      return get_text(node, source)
    end
    local inner = node:field("name")[1] or node:child(0)
    if inner then return declarator_name(inner, source) end
    return "_"
  end

  local function declarator_sig(node, source)
    local kind = node:type()
    if kind == "function_declarator" then
      local inner = node:field("declarator")[1]
      if not inner then return nil end
      local name = declarator_name(inner, source)
      local params_node = node:field("parameters")[1]
      local params = params_node and get_text(params_node, source) or "()"
      return name .. params
    elseif kind == "reference_declarator" or kind == "pointer_declarator" then
      local inner = node:field("declarator")[1] or node:child(0)
      if not inner then return nil end
      return declarator_sig(inner, source)
    else
      return declarator_name(node, source)
    end
  end

  local function method_sig(node, source)
    local ret_node = node:field("type")[1]
    local ret = ret_node and get_text(ret_node, source) or ""
    local decl_node = node:field("declarator")[1]
    if not decl_node then return nil end
    local sig = declarator_sig(decl_node, source)
    if not sig then return nil end
    local text = ret == "" and sig or (ret .. " " .. sig)
    return compact_ws(text)
  end

  local function decl_sig(node, source)
    local decl_node = node:field("declarator")[1]
    if not decl_node or decl_node:type() ~= "function_declarator" then return nil end
    return method_sig(node, source)
  end

  local function extract_class_body(node, source)
    local body = node:field("body")[1]
    if not body then return {} end
    return extract_body_members(body, source, {
      { kind = "function_definition", handler = "method",
        fn = function(n, s) return method_sig(n, s) end },
      { kind = "declaration", handler = "method",
        fn = function(n, s) return decl_sig(n, s) end },
      { kind = "field_declaration", handler = "field", counter = "field_declaration",
        fn = function(n, s)
          return compact_ws(get_text(n, s):gsub(";%s*$", ""))
        end },
    })
  end

  local function extract_class_or_struct(node, source, is_class, prefix, template_node)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local base_node = node:field("base_class_clause")[1]
    local bases = ""
    if base_node then
      local bt = get_text(base_node, source):gsub("^%s*:%s*", "")
      bases = compact_ws(" : " .. bt)
    end
    local keyword = is_class and "class" or "struct"
    local label = compact_ws((prefix and (prefix .. " ") or "") .. keyword .. " " .. name .. bases)
    local section = is_class and SECTION.Class or SECTION.Type
    local ref_node = template_node or node
    local entry = new_entry(section, ref_node, label)
    entry.children = extract_class_body(node, source)
    return entry
  end

  local function extract_template(node, source)
    local params_node = node:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "<>"
    local prefix = "template" .. params
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "function_definition" then
        local sig = method_sig(child, source)
        if sig then
          local entry = new_entry(SECTION.Function, node, compact_ws(prefix .. " " .. sig))
          return { entry }
        end
        return {}
      elseif ckind == "class_specifier" then
        local e = extract_class_or_struct(child, source, true, prefix, node)
        return e and { e } or {}
      elseif ckind == "struct_specifier" then
        local e = extract_class_or_struct(child, source, false, prefix, node)
        return e and { e } or {}
      elseif ckind == "declaration" then
        local sig = decl_sig(child, source)
        if sig then
          local entry = new_entry(SECTION.Function, node, compact_ws(prefix .. " " .. sig))
          return { entry }
        end
        return {}
      end
    end
    return {}
  end

  extract_nodes = function(node, source, _attrs)
    local kind = node:type()

    if kind == "preproc_include" then
      local path_node = node:field("path")[1]
      local raw = path_node and get_text(path_node, source) or ""
      local cleaned = raw:gsub('^["<]+', ''):gsub('[">]+$', '')
      local parts = {}
      for seg in cleaned:gmatch("[^/]+") do parts[#parts + 1] = seg end
      return { new_import_entry(node, { parts }) }

    elseif kind == "using_declaration" then
      local text = get_text(node, source)
      local cleaned = text:match("^using namespace (.+)") or text:match("^using (.+)") or text
      cleaned = cleaned:gsub(";%s*$", ""):match("^%s*(.-)%s*$")
      local parts = {}
      for seg in (cleaned .. "::"):gmatch("(.-)::") do parts[#parts + 1] = seg end
      return { new_import_entry(node, { parts }) }

    elseif kind == "namespace_definition" then
      local name_node = node:field("name")[1]
      local name = name_node and get_text(name_node, source) or "(anonymous)"
      local entry = new_entry(SECTION.Module, node, name)
      local entries = { entry }
      local body = find_child(node, "declaration_list")
      if body then
        for _, child in ipairs(body:children()) do
          local sub = extract_nodes(child, source, {})
          for _, e in ipairs(sub) do entries[#entries + 1] = e end
        end
      end
      return entries

    elseif kind == "class_specifier" then
      local e = extract_class_or_struct(node, source, true, nil, nil)
      return e and { e } or {}

    elseif kind == "struct_specifier" then
      local e = extract_class_or_struct(node, source, false, nil, nil)
      return e and { e } or {}

    elseif kind == "enum_specifier" then
      local name_node = node:field("name")[1]
      local name = name_node and get_text(name_node, source) or "(anonymous)"
      local body_node = node:field("body")[1]
      if not body_node then return {} end
      local variants = extract_enum_variants(body_node, source, "enumerator")
      local entry = new_entry(SECTION.Type, node, "enum " .. name)
      entry.children = variants
      entry.child_kind = CHILD_BRIEF
      return { entry }

    elseif kind == "function_definition" then
      local sig = method_sig(node, source)
      if not sig then return {} end
      return { new_entry(SECTION.Function, node, sig) }

    elseif kind == "template_declaration" then
      return extract_template(node, source)

    elseif kind == "preproc_def" or kind == "preproc_function_def" then
      local name_node = node:field("name")[1]
      if not name_node then return {} end
      local name = get_text(name_node, source)
      local val_node = node:field("value")[1]
      local val = val_node and truncate(get_text(val_node, source), 40) or ""
      local text = val == "" and name or compact_ws(name .. " " .. val)
      return { new_entry(SECTION.Constant, node, text) }

    elseif kind == "declaration" then
      local decl_node = node:field("declarator")[1]
      if decl_node and decl_node:type() == "function_declarator" then
        local sig = decl_sig(node, source)
        if sig then return { new_entry(SECTION.Function, node, sig) } end
      end
      return {}

    elseif kind == "type_definition" then
      local ty_node = node:field("type")[1]
      local ty = ty_node and get_text(ty_node, source) or "_"
      local decl_node = node:field("declarator")[1]
      local decl = decl_node and get_text(decl_node, source) or "_"
      return { new_entry(SECTION.Type, node, compact_ws("typedef " .. ty .. " " .. decl)) }

    elseif kind == "alias_declaration" then
      local name_node = node:field("name")[1]
      local name = name_node and get_text(name_node, source) or "_"
      local ty_node = node:field("type")[1]
      local ty = ty_node and get_text(ty_node, source) or "_"
      return { new_entry(SECTION.Type, node, compact_ws("using " .. name .. " = " .. ty)) }
    end

    return {}
  end

  return {
    extract_nodes = extract_nodes,
    import_separator = "::",
    is_doc_comment = function(node, source)
      if node:type() ~= "comment" then return false end
      local text = get_text(node, source)
      return text:sub(1, 3) == "/**" or text:sub(1, 3) == "///"
    end,
  }
end
