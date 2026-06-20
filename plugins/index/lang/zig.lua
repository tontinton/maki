return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local compact_ws = U.compact_ws
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local extract_fields_truncated = U.extract_fields_truncated

  local function get_first_identifier(node)
    for _, child in ipairs(node:children()) do
      if child:type() == "identifier" then
        return child
      end
    end
    return nil
  end

  local function extract_import_path(node, source)
    for _, child in ipairs(node:children()) do
      if child:type() == "builtin_function" then
        local builtin_id = find_child(child, "builtin_identifier")
        if builtin_id and get_text(builtin_id, source) == "@import" then
          local args = find_child(child, "arguments")
          if args then
            for _, arg in ipairs(args:children()) do
              if arg:type() == "string" then
                local raw = get_text(arg, source)
                local cleaned = raw:gsub('^"', ""):gsub('"$', "")
                local parts = {}
                for p in cleaned:gmatch("[^/]+") do
                  parts[#parts + 1] = p
                end
                if #parts > 0 then
                  return new_import_entry(node, { parts })
                end
              end
            end
          end
        end
      end
    end
    return nil
  end

  local function extract_function(node, source)
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)

    local params_node = find_child(node, "parameters")
    local params = params_node and compact_ws(get_text(params_node, source)) or "()"

    local type_node = node:field("type")[1]
    local ret = ""
    if type_node then
      local type_text = get_text(type_node, source)
      if type_text ~= "void" then
        ret = " " .. type_text
      end
    end

    return new_entry(SECTION.Function, node, name .. params .. ret)
  end

  local function extract_vardecl(node, source)
    local first_id = get_first_identifier(node)
    if not first_id then
      return nil
    end
    local name = get_text(first_id, source)

    local type_nodes = node:field("type")
    local type_str = ""
    if type_nodes and #type_nodes > 0 then
      type_str = ": " .. get_text(type_nodes[1], source)
    end

    local is_const = false
    for _, child in ipairs(node:children()) do
      if child:type() == "const" then
        is_const = true
        break
      end
    end

    local prefix = is_const and "const " or "var "
    return new_entry(SECTION.Constant, node, prefix .. name .. type_str)
  end

  local function extract_container(node, source, keyword, assigned_name)
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or assigned_name or ""
    local label = keyword .. (name ~= "" and (" " .. name) or "")
    local entry = new_entry(SECTION.Type, node, label)
    entry.children = extract_fields_truncated(node, source, "container_field", function(f, src)
      local field_name_node = f:field("name")[1]
      local field_type_node = f:field("type")[1]
      local fname = field_name_node and get_text(field_name_node, src) or "_"
      local ftype = field_type_node and get_text(field_type_node, src) or ""
      local text = ftype ~= "" and (fname .. ": " .. ftype) or fname
      return text
    end)
    return entry
  end

  local function extract_enum(node, source, assigned_name)
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or assigned_name or ""
    local label = "enum" .. (name ~= "" and (" " .. name) or "")
    local entry = new_entry(SECTION.Type, node, label)
    entry.children = extract_fields_truncated(node, source, "container_field", function(f, src)
      local field_name_node = f:field("name")[1]
      return field_name_node and get_text(field_name_node, src) or "_"
    end)
    entry.child_kind = CHILD_BRIEF
    return entry
  end

  local function extract_error_set(node, source, assigned_name)
    local name_node = node:field("name")[1]
    local name = name_node and get_text(name_node, source) or assigned_name or ""
    local label = "error" .. (name ~= "" and (" " .. name) or "")
    local entry = new_entry(SECTION.Type, node, label)
    entry.children = extract_fields_truncated(node, source, "identifier", function(f, src)
      return get_text(f, src)
    end)
    entry.child_kind = CHILD_BRIEF
    return entry
  end

  local function extract_vardecl_with_value(node, source)
    local import_entry = extract_import_path(node, source)
    if import_entry then
      return { import_entry }
    end

    local first_id = get_first_identifier(node)
    local assigned_name = first_id and get_text(first_id, source) or nil

    for _, child in ipairs(node:children()) do
      local ck = child:type()
      if ck == "struct_declaration" then
        return { extract_container(child, source, "struct", assigned_name) }
      elseif ck == "enum_declaration" then
        return { extract_enum(child, source, assigned_name) }
      elseif ck == "union_declaration" then
        return { extract_container(child, source, "union", assigned_name) }
      elseif ck == "opaque_declaration" then
        return { new_entry(SECTION.Type, node, "opaque" .. (assigned_name and (" " .. assigned_name) or "")) }
      elseif ck == "error_set_declaration" then
        return { extract_error_set(child, source, assigned_name) }
      end
    end

    local e = extract_vardecl(node, source)
    return e and { e } or {}
  end

  return {
    import_separator = "/",

    is_doc_comment = function(node, source)
      if node:type() ~= "comment" then
        return false
      end
      local text = get_text(node, source)
      return text:sub(1, 3) == "///"
    end,

    is_module_doc = function(node, source)
      if node:type() ~= "comment" then
        return false
      end
      local text = get_text(node, source)
      return text:sub(1, 3) == "//!"
    end,

    is_test_node = function(node, _source, _attrs)
      return node:type() == "test_declaration"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "function_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}
      elseif kind == "variable_declaration" then
        return extract_vardecl_with_value(node, source)
      elseif kind == "using_namespace_declaration" then
        local e = extract_import_path(node, source)
        return e and { e } or {}
      end

      return {}
    end,
  }
end
