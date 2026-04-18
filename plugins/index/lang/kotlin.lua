return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION
  local FIELD_TRUNCATE_THRESHOLD = U.FIELD_TRUNCATE_THRESHOLD
  local CHILD_BRIEF = U.CHILD_BRIEF
  local truncated_msg = U.truncated_msg

  local function modifiers_text(node, source)
    local mods = find_child(node, "modifiers")
    if not mods then return "" end
    local parts = {}
    for _, child in ipairs(mods:children()) do
      if child:type() ~= "annotation" then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    return table.concat(parts, " ")
  end

  local function tparams_text(node, source)
    local tp = find_child(node, "type_parameters")
    return tp and get_text(tp, source) or ""
  end

  local function delegation_text(node, source)
    local ds = find_child(node, "delegation_specifiers")
    return ds and (" : " .. get_text(ds, source)) or ""
  end

  local function extract_import(node, source)
    local qi = find_child(node, "qualified_identifier") or find_child(node, "identifier")
    if not qi then
      local text = get_text(node, source)
      local cleaned = text:match("^import%s+(.-)%s*$") or text
      return new_import_entry(node, { { cleaned } })
    end
    local parts = {}
    for _, child in ipairs(qi:children()) do
      if child:type() == "identifier" then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    local has_star = false
    for _, child in ipairs(node:children()) do
      if get_text(child, source) == "*" then has_star = true; break end
    end
    if has_star then parts[#parts + 1] = "*" end
    return new_import_entry(node, { parts })
  end

  local function extract_package(node, source)
    local qi = find_child(node, "qualified_identifier") or find_child(node, "identifier")
    local text
    if qi then
      text = get_text(qi, source)
    else
      text = get_text(node, source):match("^package%s+(.-)%s*$") or get_text(node, source)
    end
    return new_entry(SECTION.Module, node, text)
  end

  local function fn_sig(node, source)
    local mods = modifiers_text(node, source)
    local tparams = tparams_text(node, source)
    local name_node = node:field("simple_identifier")[1] or node:field("name")[1] or find_child(node, "identifier")
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = find_child(node, "function_value_parameters")
    local params = params_node and get_text(params_node, source) or "()"
    local ret_node = node:field("type")[1] or find_child(node, "type")
    local ret = ret_node and (" : " .. get_text(ret_node, source)) or ""
    local parts = {}
    if mods ~= "" then parts[#parts + 1] = mods end
    parts[#parts + 1] = "fun"
    if tparams ~= "" then parts[#parts + 1] = tparams end
    parts[#parts + 1] = name .. params .. ret
    return compact_ws(table.concat(parts, " "))
  end

  local function property_text(node, source)
    local mods = modifiers_text(node, source)
    local kw = "val"
    for _, child in ipairs(node:children()) do
      local t = get_text(child, source)
      if t == "var" or t == "val" then kw = t; break end
    end
    local var_decl = node:field("variable_declaration")[1] or find_child(node, "variable_declaration")
    if not var_decl then return nil end
    local id_node = var_decl:field("simple_identifier")[1] or find_child(var_decl, "simple_identifier") or find_child(var_decl, "identifier")
    local id = id_node and get_text(id_node, source) or "_"
    local type_node = var_decl:field("type")[1] or find_child(var_decl, "type")
    local type_str = type_node and (" : " .. get_text(type_node, source)) or ""
    local prefix = mods ~= "" and (mods .. " " .. kw) or kw
    return compact_ws(prefix .. " " .. id .. type_str)
  end

  local function class_members(body, source)
    local members = {}
    local prop_count = 0
    for _, child in ipairs(body:children()) do
      local ck = child:type()
      if ck == "function_declaration" then
        local sig = fn_sig(child, source)
        if sig then
          local lr = format_range(line_start(child), line_end(child))
          members[#members + 1] = sig .. " " .. lr
        end
      elseif ck == "property_declaration" then
        prop_count = prop_count + 1
        if prop_count <= FIELD_TRUNCATE_THRESHOLD then
          local text = property_text(child, source)
          if text then
            local lr = format_range(line_start(child), line_end(child))
            members[#members + 1] = text .. " " .. lr
          end
        end
      elseif ck == "companion_object" then
        local sub_body = find_child(child, "class_body")
        if sub_body then
          local sub = class_members(sub_body, source)
          for _, m in ipairs(sub) do members[#members + 1] = "companion." .. m end
        end
      elseif ck == "enum_entry" then
        local id = child:field("simple_identifier")[1]
        if id then members[#members + 1] = get_text(id, source) end
      end
    end
    if prop_count > FIELD_TRUNCATE_THRESHOLD then
      members[#members + 1] = truncated_msg(prop_count)
    end
    return members
  end

  local function extract_class(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("simple_identifier")[1] or node:field("name")[1] or find_child(node, "identifier")
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)

    local kw = "class"
    for _, child in ipairs(node:children()) do
      local t = get_text(child, source)
      if t == "interface" then kw = "interface"; break end
    end
    if mods:find("enum") then kw = "enum class" end

    local ctor_node = find_child(node, "primary_constructor")
    local ctor = ctor_node and get_text(find_child(ctor_node, "class_parameters") or ctor_node, source) or ""

    local supers = delegation_text(node, source)

    local parts = {}
    if mods ~= "" then parts[#parts + 1] = mods end
    parts[#parts + 1] = kw
    if tparams ~= "" then parts[#parts + 1] = tparams end
    parts[#parts + 1] = name .. ctor .. supers
    local label = compact_ws(table.concat(parts, " "))

    local entry = new_entry(SECTION.Class, node, label)
    local body = find_child(node, "class_body") or find_child(node, "enum_class_body")
    if body then
      entry.children = class_members(body, source)
      if kw == "enum class" then entry.child_kind = CHILD_BRIEF end
    end
    return entry
  end

  local function extract_object(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("simple_identifier")[1] or node:field("name")[1] or find_child(node, "identifier")
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local supers = delegation_text(node, source)
    local prefix = mods ~= "" and (mods .. " object ") or "object "
    local entry = new_entry(SECTION.Class, node, compact_ws(prefix .. name .. supers))
    local body = find_child(node, "class_body")
    if body then entry.children = class_members(body, source) end
    return entry
  end

  local function extract_type_alias(node, source)
    local vis_node = find_child(node, "modifiers")
    local vis = vis_node and get_text(vis_node, source) or ""
    local name_node = node:field("type_alias_name")[1] or node:field("name")[1] or find_child(node, "identifier")
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = tparams_text(node, source)
    local rhs = nil
    for i = node:child_count() - 1, 0, -1 do
      local c = node:child(i)
      if c:type() ~= "simple_identifier" and c:type() ~= "=" and c:type() ~= "type_alias" then
        rhs = get_text(c, source)
        break
      end
    end
    local rhs_str = rhs and (" = " .. rhs) or ""
    local label = compact_ws((vis ~= "" and (vis .. " ") or "") .. "typealias " .. name .. tparams .. rhs_str)
    return new_entry(SECTION.Type, node, label)
  end

  local function extract_property(node, source)
    local mods = modifiers_text(node, source)
    local var_decl = node:field("variable_declaration")[1] or find_child(node, "variable_declaration")
    if not var_decl then return nil end
    local id_node = var_decl:field("simple_identifier")[1] or find_child(var_decl, "simple_identifier") or find_child(var_decl, "identifier")
    local id = id_node and get_text(id_node, source) or ""
    if not (mods:find("const") or id:match("^[A-Z_][A-Z0-9_]*$")) then return nil end
    local text = property_text(node, source)
    return text and new_entry(SECTION.Constant, node, text) or nil
  end

  local function extract_declaration(node, source)
    local ck = node:type()
    if ck == "class_declaration" then
      return extract_class(node, source)
    elseif ck == "object_declaration" then
      return extract_object(node, source)
    elseif ck == "function_declaration" then
      local sig = fn_sig(node, source)
      return sig and new_entry(SECTION.Function, node, sig) or nil
    elseif ck == "property_declaration" then
      return extract_property(node, source)
    elseif ck == "type_alias" then
      return extract_type_alias(node, source)
    end
    return nil
  end

  local DECL_KINDS = {
    class_declaration = true, object_declaration = true,
    function_declaration = true, property_declaration = true, type_alias = true,
  }

  return {
    import_separator = ".",

    is_doc_comment = function(node, source)
      if node:type() ~= "multiline_comment" then return false end
      return get_text(node, source):sub(1, 3) == "/**"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()
      if kind == "import_header" or kind == "import" then
        return { extract_import(node, source) }
      elseif kind == "import_list" then
        local results = {}
        for _, child in ipairs(node:children()) do
          if child:type() == "import_header" or child:type() == "import" then
            results[#results + 1] = extract_import(child, source)
          end
        end
        return results
      elseif kind == "package_header" then
        return { extract_package(node, source) }
      elseif DECL_KINDS[kind] then
        local e = extract_declaration(node, source)
        return e and { e } or {}
      elseif kind == "statements" then
        local results = {}
        for _, child in ipairs(node:children()) do
          if DECL_KINDS[child:type()] then
            local e = extract_declaration(child, source)
            if e then results[#results + 1] = e end
          end
        end
        return results
      end
      return {}
    end,
  }
end
