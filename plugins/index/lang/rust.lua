return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local truncated_msg = U.truncated_msg
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local expand_import = U.expand_import
  local SECTION = U.SECTION
  local FIELD_TRUNCATE_THRESHOLD = U.FIELD_TRUNCATE_THRESHOLD
  local CHILD_BRIEF = U.CHILD_BRIEF
  local prefixed = U.prefixed

  local function vis_prefix(node, source)
    for _, child in ipairs(node:children()) do
      if child:type() == "visibility_modifier" then
        return get_text(child, source)
      end
    end
    return ""
  end

  local function relevant_attr_texts(attrs, source)
    local result = {}
    for _, a in ipairs(attrs) do
      local text = get_text(a, source)
      if text:find("derive") or text:find("cfg") then
        result[#result + 1] = text
      end
    end
    return result
  end

  local function fn_signature(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = find_child(node, "parameters")
    local params = params_node and get_text(params_node, source) or "()"
    local ret_node = node:field("return_type")[1]
    local ret = ""
    if ret_node then
      local t = get_text(ret_node, source)
      if t:sub(1, 2) == "->" then
        ret = " " .. t
      else
        ret = " -> " .. t
      end
    end
    return compact_ws(name .. params .. ret)
  end

  local function extract_fields(node, source)
    local body = find_child(node, "field_declaration_list")
      or find_child(node, "enum_variant_list")
    if not body then return {} end
    local fields = {}
    local total = 0
    for _, child in ipairs(body:children()) do
      local ckind = child:type()
      if ckind == "field_declaration" then
        total = total + 1
        local v = vis_prefix(child, source)
        local fname = child:field("name")[1]
        local fname_text = fname and get_text(fname, source) or "_"
        local ftype = child:field("type")[1]
        local ftype_text = ftype and get_text(ftype, source) or "_"
        if total <= FIELD_TRUNCATE_THRESHOLD or v ~= "" then
          fields[#fields + 1] = prefixed(v, fname_text .. ": " .. ftype_text)
        end
      elseif ckind == "enum_variant" then
        total = total + 1
        if total <= FIELD_TRUNCATE_THRESHOLD then
          local vname = child:field("name")[1]
          fields[#fields + 1] = vname and get_text(vname, source) or "_"
        end
      end
    end
    if total > FIELD_TRUNCATE_THRESHOLD and #fields < total then
      fields[#fields + 1] = truncated_msg(total)
    end
    return fields
  end

  local function extract_methods(node, source, include_vis)
    local body = find_child(node, "declaration_list")
    if not body then return {} end
    local methods = {}
    for _, child in ipairs(body:children()) do
      local ckind = child:type()
      if ckind == "function_item" or ckind == "function_signature_item" then
        local sig = fn_signature(child, source)
        if sig then
          local lr = format_range(line_start(child), line_end(child))
          if include_vis then
            local v = vis_prefix(child, source)
            methods[#methods + 1] = prefixed(v, sig) .. " " .. lr
          else
            methods[#methods + 1] = sig .. " " .. lr
          end
        end
      end
    end
    return methods
  end

  return {
    import_separator = "::",

    is_attr = function(node)
      return node:type() == "attribute_item"
    end,

    is_doc_comment = function(node, source)
      if node:type() ~= "line_comment" then return false end
      local text = get_text(node, source)
      return text:sub(1, 3) == "///" and text:sub(1, 4) ~= "////"
    end,

    is_module_doc = function(node, source)
      if node:type() ~= "line_comment" then return false end
      local text = get_text(node, source)
      return text:sub(1, 3) == "//!"
    end,

    is_test_node = function(node, source, attrs)
      local kind = node:type()
      if kind ~= "mod_item" and kind ~= "function_item" then return false end
      for _, a in ipairs(attrs) do
        local text = get_text(a, source)
        if text == "#[test]" or text == "#[cfg(test)]" or text:sub(-7) == "::test]" then
          return true
        end
      end
      return false
    end,

    extract_nodes = function(node, source, attrs)
      local kind = node:type()

      if kind == "use_declaration" then
        local tree = find_child(node, "use_declaration") or node
        local argument = find_child(tree, "scoped_identifier")
          or find_child(tree, "use_wildcard")
          or find_child(tree, "use_list")
          or find_child(tree, "scoped_use_list")
          or find_child(tree, "identifier")
        local text
        if argument then
          text = get_text(argument, source)
        else
          local full = get_text(node, source)
          text = full:match("^use (.-)%;?$") or full:gsub(";$", "")
        end
        return { new_import_entry(node, expand_import(text, "::")) }

      elseif kind == "struct_item" or kind == "enum_item" or kind == "union_item" then
        local v = vis_prefix(node, source)
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local name = get_text(name_node, source)
        local generics_node = find_child(node, "type_parameters")
        local generics = generics_node and get_text(generics_node, source) or ""
        local kind_str = kind:gsub("_item", ""):gsub("_definition", "")
        local text = prefixed(v, kind_str .. " " .. name .. generics)
        local is_enum = find_child(node, "enum_variant_list") ~= nil
        local entry = new_entry(SECTION.Type, node, text)
        entry.children = extract_fields(node, source)
        entry.attrs = relevant_attr_texts(attrs, source)
        if is_enum then entry.child_kind = CHILD_BRIEF end
        return { entry }

      elseif kind == "function_item" then
        local v = vis_prefix(node, source)
        local sig = fn_signature(node, source)
        if not sig then return {} end
        return { new_entry(SECTION.Function, node, prefixed(v, sig)) }

      elseif kind == "trait_item" then
        local v = vis_prefix(node, source)
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local name = get_text(name_node, source)
        local generics_node = find_child(node, "type_parameters")
        local generics = generics_node and get_text(generics_node, source) or ""
        local entry = new_entry(SECTION.Trait, node, prefixed(v, name .. generics))
        entry.children = extract_methods(node, source, false)
        return { entry }

      elseif kind == "impl_item" then
        local type_node = node:field("type")[1] or find_child(node, "type_identifier")
        if not type_node then return {} end
        local type_name = get_text(type_node, source)
        local trait_node = node:field("trait")[1]
        local text
        if trait_node then
          text = get_text(trait_node, source) .. " for " .. type_name
        else
          text = type_name
        end
        local entry = new_entry(SECTION.Impl, node, text)
        entry.children = extract_methods(node, source, true)
        return { entry }

      elseif kind == "const_item" or kind == "static_item" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local name = get_text(name_node, source)
        local type_node = node:field("type")[1]
        local type_str = type_node and (": " .. get_text(type_node, source)) or ""
        local v = vis_prefix(node, source)
        local prefix = kind == "static_item" and "static " or ""
        return { new_entry(SECTION.Constant, node, prefixed(v, prefix .. name .. type_str)) }

      elseif kind == "mod_item" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local v = vis_prefix(node, source)
        return { new_entry(SECTION.Module, node, prefixed(v, get_text(name_node, source))) }

      elseif kind == "macro_definition" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        return { new_entry(SECTION.Macro, node, get_text(name_node, source) .. "!") }

      elseif kind == "type_item" then
        local v = vis_prefix(node, source)
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local name = get_text(name_node, source)
        local val_node = node:field("type")[1]
        local val = val_node and (" = " .. get_text(val_node, source)) or ""
        return { new_entry(SECTION.Type, node, prefixed(v, "type " .. name .. val)) }
      end

      return {}
    end,
  }
end
