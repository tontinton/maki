return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local expand_import = U.expand_import
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local prefixed = U.prefixed
  local extract_body_members = U.extract_body_members
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end

  local MODIFIER_KINDS = {
    access_modifier = true, case = true, abstract = true, sealed = true,
    final = true, implicit = true, lazy = true, override = true,
    open = true,
  }

  local function modifiers_text(node, source)
    local mods = find_child(node, "modifiers")
    if not mods then return "" end
    local parts = {}
    for _, child in ipairs(mods:children()) do
      if MODIFIER_KINDS[child:type()] then
        parts[#parts + 1] = get_text(child, source)
      end
    end
    return table.concat(parts, " ")
  end

  local function type_params_text(node, source)
    local tp = node:field("type_parameters")[1]
    return tp and get_text(tp, source) or ""
  end

  local function extends_text(node, source)
    local ext = node:field("extend")[1]
    return ext and (" " .. get_text(ext, source)) or ""
  end

  local function fn_sig(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = type_params_text(node, source)
    local params_node = find_child(node, "parameters")
    local params = params_node and get_text(params_node, source) or "()"
    local ret_node = node:field("return_type")[1]
    local ret = ret_node and (": " .. get_text(ret_node, source)) or ""
    return compact_ws(prefixed(mods, "def " .. name .. tparams .. params .. ret))
  end

  local function val_text(node, source)
    local mods = modifiers_text(node, source)
    local keyword = "val"
    for _, child in ipairs(node:children()) do
      local t = child:type()
      if t == "val" or t == "var" then keyword = t; break end
    end
    local pat_node = node:field("pattern")[1]
    local pat = pat_node and get_text(pat_node, source) or "_"
    local type_node = node:field("type")[1]
    local type_str = type_node and (": " .. get_text(type_node, source)) or ""
    return truncate(compact_ws(prefixed(mods, keyword .. " " .. pat .. type_str)), 80)
  end

  local CLASS_RULES = {
    { kind = "function_definition",  handler = "method", fn = fn_sig },
    { kind = "function_declaration", handler = "method", fn = fn_sig },
    { kind = "val_definition",       handler = "field",  fn = val_text, counter = "val" },
    { kind = "var_definition",       handler = "field",  fn = val_text, counter = "val" },
    { kind = "val_declaration",      handler = "field",  fn = val_text, counter = "val" },
    { kind = "var_declaration",      handler = "field",  fn = val_text, counter = "val" },
  }

  local function extract_class_like(node, source, keyword, section)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = type_params_text(node, source)
    local ext = extends_text(node, source)
    local text = compact_ws(prefixed(mods, keyword .. " " .. name .. tparams .. ext))
    local entry = new_entry(section, node, text)
    local body = find_child(node, "template_body")
    if body then
      entry.children = extract_body_members(body, source, CLASS_RULES)
    end
    return entry
  end

  local function extract_function(node, source)
    local sig = fn_sig(node, source)
    if not sig then return nil end
    return new_entry(SECTION.Function, node, sig)
  end

  local function extract_val(node, source)
    local text = val_text(node, source)
    return new_entry(SECTION.Constant, node, text)
  end

  local function extract_type_def(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = type_params_text(node, source)
    local type_node = node:field("type")[1]
    local type_str = type_node and ("= " .. get_text(type_node, source)) or ""
    return new_entry(SECTION.Type, node, compact_ws(prefixed(mods, "type " .. name .. tparams .. " " .. type_str)))
  end

  local function extract_enum(node, source)
    local mods = modifiers_text(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local tparams = type_params_text(node, source)
    local ext = extends_text(node, source)
    local text = compact_ws(prefixed(mods, name .. tparams .. ext))
    local entry = new_entry(SECTION.Type, node, text)
    local body = find_child(node, "enum_body")
    if body then
      local cases = {}
      for _, child in ipairs(body:children()) do
        if child:type() == "enum_case_definitions" then
          for _, cc in ipairs(child:children()) do
            if cc:type() == "simple_enum_case" or cc:type() == "full_enum_case" then
              local cn = cc:field("name")[1]
              if cn then cases[#cases + 1] = get_text(cn, source) end
            end
          end
        end
      end
      entry.children = cases
      entry.child_kind = CHILD_BRIEF
    end
    return entry
  end

  local function extract_import(node, source)
    local text = get_text(node, source)
    local cleaned = text:match("^import (.+)") or text
    cleaned = cleaned:gsub(";%s*$", ""):match("^%s*(.-)%s*$")
    return new_import_entry(node, expand_import(cleaned, "."))
  end

  local function extract_package(node, source)
    local name_node = node:field("name")[1]
    local name
    if name_node then
      name = get_text(name_node, source)
    else
      local text = get_text(node, source)
      name = text:match("^package (.+)") or text
      name = name:match("^%s*(.-)%s*$")
    end
    return new_entry(SECTION.Module, node, name)
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
        return { extract_import(node, source) }
      elseif kind == "package_clause" then
        local e = extract_package(node, source)
        return e and { e } or {}
      elseif kind == "class_definition" then
        local e = extract_class_like(node, source, "class", SECTION.Class)
        return e and { e } or {}
      elseif kind == "object_definition" then
        local e = extract_class_like(node, source, "object", SECTION.Class)
        return e and { e } or {}
      elseif kind == "trait_definition" then
        local e = extract_class_like(node, source, "trait", SECTION.Trait)
        return e and { e } or {}
      elseif kind == "function_definition" or kind == "function_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}
      elseif kind == "val_definition" or kind == "val_declaration"
          or kind == "var_definition" or kind == "var_declaration" then
        local e = extract_val(node, source)
        return e and { e } or {}
      elseif kind == "type_definition" then
        local e = extract_type_def(node, source)
        return e and { e } or {}
      elseif kind == "enum_definition" then
        local e = extract_enum(node, source)
        return e and { e } or {}
      end
      return {}
    end,
  }
end
