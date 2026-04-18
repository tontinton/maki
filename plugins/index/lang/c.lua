return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local extract_enum_variants = U.extract_enum_variants
  local extract_fields_truncated = U.extract_fields_truncated

  local DEFINE_TRUNCATE = 40

  local function extract_include(node, source)
    local path_node = node:field("path")[1]
    if not path_node then return nil end
    local raw = get_text(path_node, source)
    local cleaned = raw:gsub("[<>\"']", "")
    local parts = {}
    for p in cleaned:gmatch("[^/]+") do parts[#parts + 1] = p end
    return new_import_entry(node, { parts })
  end

  local function unwrap_to_fn_declarator(node)
    if not node then return nil end
    local ck = node:type()
    if ck == "function_declarator" then return node end
    if ck == "pointer_declarator" then
      local inner = node:field("declarator")[1]
      return unwrap_to_fn_declarator(inner)
    end
    return nil
  end

  local function build_fn_sig(node, source)
    local decl_node = node:field("declarator")[1]
    local fn_decl = unwrap_to_fn_declarator(decl_node)
    if not fn_decl then return nil end
    local name_node = fn_decl:field("declarator")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = fn_decl:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "()"
    local ret_node = node:field("type")[1]
    local ret = ret_node and (get_text(ret_node, source) .. " ") or ""
    return compact_ws(ret .. name .. params)
  end

  local function field_format(child, source)
    local raw = get_text(child, source)
    raw = raw:gsub(";%s*$", "")
    local lr = format_range(line_start(child), line_end(child))
    return compact_ws(raw):match("^%s*(.-)%s*$") .. " " .. lr
  end

  local function extract_struct(node, source)
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or ""
    local label = "struct" .. (name ~= "" and (" " .. name) or "")
    local body = find_child(node, "field_declaration_list")
    local entry = new_entry(SECTION.Type, node, label)
    if body then
      entry.children = extract_fields_truncated(body, source, "field_declaration", field_format)
    end
    return entry
  end

  local function extract_enum(node, source)
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or ""
    local label = "enum" .. (name ~= "" and (" " .. name) or "")
    local body = find_child(node, "enumerator_list")
    local entry = new_entry(SECTION.Type, node, label)
    if body then
      entry.children = extract_enum_variants(body, source, "enumerator")
      entry.child_kind = CHILD_BRIEF
    end
    return entry
  end

  local function extract_typedef(node, source)
    local type_node = node:field("type")[1]
    if not type_node then return nil end
    local decl = node:field("declarator")[1]
    local alias = decl and get_text(decl, source) or ""
    local ck = type_node:type()
    if ck == "struct_specifier" or ck == "union_specifier" then
      local body = find_child(type_node, "field_declaration_list")
      if body then
        local inner_name_node = type_node:field("name")[1]
        local inner_name = inner_name_node and get_text(inner_name_node, source) or ""
        local keyword = ck == "union_specifier" and "union" or "struct"
        local inner = inner_name == "" and keyword or (keyword .. " " .. inner_name)
        local label = "typedef " .. inner .. " " .. alias
        local e = new_entry(SECTION.Type, node, label)
        e.children = extract_fields_truncated(body, source, "field_declaration", field_format)
        return e
      else
        local e = extract_struct(type_node, source)
        return e
      end
    elseif ck == "enum_specifier" then
      local body = find_child(type_node, "enumerator_list")
      if body then
        local inner_name_node = type_node:field("name")[1]
        local inner_name = inner_name_node and get_text(inner_name_node, source) or ""
        local keyword = "enum"
        local inner = inner_name == "" and keyword or (keyword .. " " .. inner_name)
        local label = "typedef " .. inner .. " " .. alias
        local e = new_entry(SECTION.Type, node, label)
        e.children = extract_enum_variants(body, source, "enumerator")
        e.child_kind = CHILD_BRIEF
        return e
      else
        local e = extract_enum(type_node, source)
        return e
      end
    else
      local type_text = get_text(type_node, source)
      local decl_text = decl and get_text(decl, source) or ""
      return new_entry(SECTION.Type, node, compact_ws("typedef " .. type_text .. " " .. decl_text))
    end
  end

  local function extract_define(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local val_node = node:field("value")[1]
    local val_str = val_node and (" " .. truncate(get_text(val_node, source), DEFINE_TRUNCATE)) or ""
    return new_entry(SECTION.Constant, node, name .. val_str)
  end

  local function extract_declaration(node, source)
    local decl_node = node:field("declarator")[1]
    if not decl_node then return nil end
    local fn_decl = unwrap_to_fn_declarator(decl_node)
    if not fn_decl then return nil end
    local sig = build_fn_sig(node, source)
    if not sig then return nil end
    return new_entry(SECTION.Function, node, sig)
  end

  return {
    import_separator = "/",

    is_doc_comment = function(node, source)
      local ck = node:type()
      if ck ~= "comment" then return false end
      local text = get_text(node, source)
      return text:sub(1, 3) == "/**" or text:sub(1, 3) == "///"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "preproc_include" then
        local e = extract_include(node, source)
        return e and { e } or {}
      elseif kind == "preproc_def" then
        local e = extract_define(node, source)
        return e and { e } or {}
      elseif kind == "preproc_function_def" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local name = get_text(name_node, source)
        local params_node = node:field("parameters")[1]
        local params = params_node and get_text(params_node, source) or ""
        return { new_entry(SECTION.Macro, node, name .. params) }
      elseif kind == "function_definition" then
        local sig = build_fn_sig(node, source)
        return sig and { new_entry(SECTION.Function, node, sig) } or {}
      elseif kind == "struct_specifier" then
        return { extract_struct(node, source) }
      elseif kind == "union_specifier" then
        local e = extract_struct(node, source)
        e.text = e.text:gsub("^struct", "union")
        return { e }
      elseif kind == "enum_specifier" then
        return { extract_enum(node, source) }
      elseif kind == "type_definition" then
        local e = extract_typedef(node, source)
        return e and { e } or {}
      elseif kind == "declaration" then
        local e = extract_declaration(node, source)
        return e and { e } or {}
      end
      return {}
    end,
  }
end
