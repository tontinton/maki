return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local truncate = U.truncate
  local extract_fields_truncated = U.extract_fields_truncated
  local SECTION = U.SECTION

  local function strip_quotes(s)
    return s:match('^["\'](.+)["\']$') or s
  end

  local function split_path(s, sep)
    local path = {}
    for part in s:gmatch("[^" .. sep .. "]+") do path[#path + 1] = part end
    return path
  end

  local function params_result(node, source)
    local params_node = node:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "()"
    local result_node = node:field("result")[1]
    local result = result_node and (" " .. get_text(result_node, source)) or ""
    return compact_ws(params .. result)
  end

  local function extract_import(node, source)
    local list = find_child(node, "import_spec_list")
    local paths = {}
    if list then
      for _, child in ipairs(list:children()) do
        if child:type() == "import_spec" then
          local path_node = child:field("path")[1]
          if path_node then
            paths[#paths + 1] = split_path(strip_quotes(get_text(path_node, source)), "/")
          end
        end
      end
    else
      local spec = find_child(node, "import_spec")
      if spec then
        local path_node = spec:field("path")[1]
        if path_node then
          paths[#paths + 1] = split_path(strip_quotes(get_text(path_node, source)), "/")
        end
      end
    end
    if #paths == 0 then return nil end
    return new_import_entry(node, paths)
  end

  local function extract_function(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local sig = get_text(name_node, source) .. params_result(node, source)
    return new_entry(SECTION.Function, node, sig)
  end

  local function extract_method(node, source)
    local recv_node = node:field("receiver")[1]
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local recv = recv_node and (get_text(recv_node, source) .. " ") or ""
    local sig = recv .. get_text(name_node, source) .. params_result(node, source)
    return new_entry(SECTION.Impl, node, sig)
  end

  local function extract_type_declaration(node, source)
    local entries = {}
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      if ckind == "type_spec" then
        local name_node = child:field("name")[1]
        if name_node then
          local name = get_text(name_node, source)
          local type_node = child:field("type")[1]
          if type_node then
            local ttype = type_node:type()
            if ttype == "struct_type" then
              local body = find_child(type_node, "field_declaration_list")
              local entry = new_entry(SECTION.Type, child, "struct " .. name)
              if body then
                entry.children = extract_fields_truncated(body, source, "field_declaration", function(f, src)
                  local field_name_node = f:field("name")[1]
                  local field_type_node = f:field("type")[1]
                  local fname = field_name_node and get_text(field_name_node, src) or "_"
                  local ftype = field_type_node and get_text(field_type_node, src) or "_"
                  return fname .. " " .. ftype
                end)
              end
              entries[#entries + 1] = entry
            elseif ttype == "interface_type" then
              local entry = new_entry(SECTION.Type, child, "interface " .. name)
              local members = {}
              for _, m in ipairs(type_node:children()) do
                local mkind = m:type()
                if mkind == "method_elem" then
                  local mname_node = m:field("name")[1]
                  if mname_node then
                    local mname = get_text(mname_node, source)
                    local msig = mname .. params_result(m, source)
                    local lr = format_range(line_start(m), line_end(m))
                    members[#members + 1] = compact_ws(msig) .. " " .. lr
                  end
                elseif mkind == "type_elem" then
                  local lr = format_range(line_start(m), line_end(m))
                  members[#members + 1] = truncate(get_text(m, source), 60) .. " " .. lr
                end
              end
              entry.children = members
              entries[#entries + 1] = entry
            else
              local val = truncate(get_text(type_node, source), 60)
              entries[#entries + 1] = new_entry(SECTION.Type, child, name .. " " .. val)
            end
          end
        end
      elseif ckind == "type_alias" then
        local name_node = child:field("name")[1]
        local type_node = child:field("type")[1]
        if name_node and type_node then
          local text = "type " .. get_text(name_node, source) .. " = " .. get_text(type_node, source)
          entries[#entries + 1] = new_entry(SECTION.Type, child, text)
        end
      end
    end
    return entries
  end

  local function extract_const_var(node, source, is_var)
    local entries = {}
    local list_kind = is_var and "var_spec_list" or "const_spec_list"
    local spec_kind = is_var and "var_spec" or "const_spec"
    local list = find_child(node, list_kind) or node
    for _, child in ipairs(list:children()) do
      if child:type() == spec_kind then
        local name_node = child:field("name")[1]
        if name_node then
          local name = get_text(name_node, source)
          local type_node = child:field("type")[1]
          local type_str = type_node and (" " .. get_text(type_node, source)) or ""
          local prefix = is_var and "var " or ""
          entries[#entries + 1] = new_entry(SECTION.Constant, child, prefix .. name .. type_str)
        end
      end
    end
    return entries
  end

  return {
    import_separator = "/",
    is_doc_comment = function(node, _source) return node:type() == "comment" end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "import_declaration" then
        local e = extract_import(node, source)
        return e and { e } or {}

      elseif kind == "function_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}

      elseif kind == "method_declaration" then
        local e = extract_method(node, source)
        return e and { e } or {}

      elseif kind == "type_declaration" then
        return extract_type_declaration(node, source)

      elseif kind == "const_declaration" then
        return extract_const_var(node, source, false)

      elseif kind == "var_declaration" then
        return extract_const_var(node, source, true)
      end

      return {}
    end,
  }
end
