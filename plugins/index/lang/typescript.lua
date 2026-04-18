return function(U)
  local function return_type(node, source)
    local r = U.get_text(node, source)
    if r:sub(1, 1) == ":" then return r end
    return ": " .. r
  end

  local function class_member_sig(node, source)
    local mn_node = node:field("name")[1]
    local mn = mn_node and U.get_text(mn_node, source) or "_"
    local params_node = node:field("parameters")[1]
    local params = params_node and U.get_text(params_node, source) or ""
    local ret_node = node:field("return_type")[1]
    local ret = ret_node and return_type(ret_node, source) or ""
    return mn .. params .. ret
  end

  local function is_exported(node)
    local parent = node:parent()
    return parent and parent:type() == "export_statement"
  end

  local function export_prefix(node)
    if is_exported(node) then return "export " end
    return ""
  end

  local function extract_import(node, source)
    local text = U.get_text(node, source)
    local cleaned = text:match("^import (.+)") or text
    cleaned = cleaned:gsub(";%s*$", "")
    return U.new_import_entry(node, { { cleaned } })
  end

  local function extract_class(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = U.get_text(name_node, source)
    local body_node = node:field("body")[1]
    if not body_node then return nil end
    local methods = {}
    local field_counts = {}
    for _, child in ipairs(body_node:children()) do
      local ckind = child:type()
      if ckind == "method_definition" then
        local sig = class_member_sig(child, source)
        if sig then
          local lr = U.format_range(U.line_start(child), U.line_end(child))
          methods[#methods + 1] = sig .. " " .. lr
        end
      elseif ckind == "public_field_definition" or ckind == "property_definition" then
        local counter = "field"
        field_counts[counter] = (field_counts[counter] or 0) + 1
        if field_counts[counter] <= U.FIELD_TRUNCATE_THRESHOLD then
          local sig = class_member_sig(child, source)
          if sig then
            local lr = U.format_range(U.line_start(child), U.line_end(child))
            methods[#methods + 1] = sig .. " " .. lr
          end
        end
      end
    end
    for _, count in pairs(field_counts) do
      if count > U.FIELD_TRUNCATE_THRESHOLD then
        methods[#methods + 1] = U.truncated_msg(count)
      end
    end
    local entry = U.new_entry(U.SECTION.Class, node, export_prefix(node) .. name)
    entry.children = methods
    return entry
  end

  local function extract_function(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = U.get_text(name_node, source)
    local params_node = node:field("parameters")[1]
    local params = params_node and U.get_text(params_node, source) or "()"
    local ret_node = node:field("return_type")[1]
    local ret_str = ret_node and return_type(ret_node, source) or ""
    local ep = export_prefix(node)
    return U.new_entry(U.SECTION.Function, node, U.compact_ws(ep .. name .. params .. ret_str))
  end

  local function extract_interface(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = U.get_text(name_node, source)
    local body_node = node:field("body")[1]
    if not body_node then return nil end
    local fields = {}
    for _, child in ipairs(body_node:children()) do
      local ckind = child:type()
      if ckind == "property_signature" or ckind == "method_signature" then
        local text = U.get_text(child, source)
        text = text:gsub("[,;]%s*$", "")
        fields[#fields + 1] = text
      end
    end
    local entry = U.new_entry(U.SECTION.Type, node, export_prefix(node) .. "interface " .. name)
    entry.children = fields
    return entry
  end

  local function extract_type_alias(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = U.get_text(name_node, source)
    local val_node = node:field("value")[1]
    local val_str = val_node and (" = " .. U.truncate(U.get_text(val_node, source), 80)) or ""
    return U.new_entry(U.SECTION.Type, node, export_prefix(node) .. "type " .. name .. val_str)
  end

  local function extract_enum(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = U.get_text(name_node, source)
    return U.new_entry(U.SECTION.Type, node, export_prefix(node) .. "enum " .. name)
  end

  local function extract_const(node, source)
    local decl = U.find_child(node, "variable_declarator")
    if not decl then return nil end
    local name_node = decl:field("name")[1]
    if not name_node then return nil end
    local name = U.get_text(name_node, source)
    local type_node = decl:field("type")[1]
    local type_str = type_node and return_type(type_node, source) or ""
    local val_node = decl:field("value")[1]
    local val_str = val_node and (" = " .. U.truncate(U.get_text(val_node, source), 60)) or ""
    return U.new_entry(U.SECTION.Constant, node, export_prefix(node) .. name .. type_str .. val_str)
  end

  local function extract_lexical_declaration(node, source)
    local first = node:child(0)
    if not first then return nil end
    if U.get_text(first, source) == "const" then
      return extract_const(node, source)
    end
    return nil
  end

  local function extract_export_statement(node, source)
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "class_declaration" then return extract_class(child, source)
      elseif ckind == "function_declaration" then return extract_function(child, source)
      elseif ckind == "interface_declaration" then return extract_interface(child, source)
      elseif ckind == "type_alias_declaration" then return extract_type_alias(child, source)
      elseif ckind == "lexical_declaration" then return extract_lexical_declaration(child, source)
      elseif ckind == "enum_declaration" then return extract_enum(child, source)
      end
    end
    return nil
  end

  local function extract_node(node, source)
    local kind = node:type()
    if kind == "import_statement" then
      return { extract_import(node, source) }
    elseif kind == "class_declaration" then
      local e = extract_class(node, source)
      return e and { e } or {}
    elseif kind == "function_declaration" then
      local e = extract_function(node, source)
      return e and { e } or {}
    elseif kind == "interface_declaration" then
      local e = extract_interface(node, source)
      return e and { e } or {}
    elseif kind == "type_alias_declaration" then
      local e = extract_type_alias(node, source)
      return e and { e } or {}
    elseif kind == "enum_declaration" then
      local e = extract_enum(node, source)
      return e and { e } or {}
    elseif kind == "lexical_declaration" then
      local e = extract_lexical_declaration(node, source)
      return e and { e } or {}
    elseif kind == "export_statement" then
      local e = extract_export_statement(node, source)
      return e and { e } or {}
    end
    return {}
  end

  return {
    extract_nodes = function(node, source, _attrs) return extract_node(node, source) end,
    import_separator = "/",
    is_doc_comment = function(node, source)
      return node:type() == "comment" and U.get_text(node, source):sub(1, 3) == "/**"
    end,
  }
end
