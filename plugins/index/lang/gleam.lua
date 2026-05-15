return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local compact_ws = U.compact_ws
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local extract_fields_truncated = U.extract_fields_truncated

  local function extract_import(node, source)
    local module_nodes = node:field("module")
    if not module_nodes or #module_nodes == 0 then
      return nil
    end
    local module_node = module_nodes[1]

    local module_text = get_text(module_node, source)
    local parts = {}
    for part in module_text:gmatch("[^/]+") do
      parts[#parts + 1] = part
    end

    local paths = {}

    local imports_nodes = node:field("imports")
    local alias_nodes = node:field("alias")

    if imports_nodes and #imports_nodes > 0 then
      local names = {}
      for _, child in ipairs(imports_nodes[1]:children()) do
        if child:type() == "unqualified_import" then
          local name_nodes = child:field("name")
          if name_nodes and #name_nodes > 0 then
            local name = get_text(name_nodes[1], source)
            local alias = child:field("alias")
            if alias and #alias > 0 then
              name = name .. " as " .. get_text(alias[1], source)
            end
            names[#names + 1] = name
          end
        end
      end
      if #names > 0 then
        local child_parts = {}
        for _, p in ipairs(parts) do
          child_parts[#child_parts + 1] = p
        end
        local label = table.concat(names, ", ")
        if alias_nodes and #alias_nodes > 0 then
          label = label .. " as " .. get_text(alias_nodes[1], source)
        end
        child_parts[#child_parts + 1] = label
        paths[#paths + 1] = child_parts
      end
    elseif alias_nodes and #alias_nodes > 0 then
      local child_parts = {}
      for _, p in ipairs(parts) do
        child_parts[#child_parts + 1] = p
      end
      child_parts[#child_parts + 1] = "as " .. get_text(alias_nodes[1], source)
      paths[#paths + 1] = child_parts
    else
      paths[#paths + 1] = parts
    end

    return new_import_entry(node, paths)
  end

  local function extract_constant(node, source)
    local name_nodes = node:field("name")
    if not name_nodes or #name_nodes == 0 then
      return nil
    end
    local name = get_text(name_nodes[1], source)

    local type_nodes = node:field("type")
    local type_str = ""
    if type_nodes and #type_nodes > 0 then
      type_str = ": " .. compact_ws(get_text(type_nodes[1], source))
    end

    return new_entry(SECTION.Constant, node, "const " .. name .. type_str)
  end

  local function extract_function(node, source, prefix)
    local name_nodes = node:field("name")
    if not name_nodes or #name_nodes == 0 then
      return nil
    end
    local name = get_text(name_nodes[1], source)

    local params_nodes = node:field("parameters")
    local params = params_nodes and #params_nodes > 0 and compact_ws(get_text(params_nodes[1], source)) or "()"

    local ret = ""
    local ret_nodes = node:field("return_type")
    if ret_nodes and #ret_nodes > 0 then
      ret = " -> " .. compact_ws(get_text(ret_nodes[1], source))
    end

    return new_entry(SECTION.Function, node, prefix .. name .. params .. ret)
  end

  local function type_label(tn, source)
    local name_nodes = tn:field("name")
    local name = (name_nodes and #name_nodes > 0) and get_text(name_nodes[1], source) or ""
    local tp = find_child(tn, "type_parameters")
    return name .. (tp and get_text(tp, source) or "")
  end

  local function extract_type_definition(node, source)
    local is_opaque = false
    for _, child in ipairs(node:children()) do
      if child:type() == "opacity_modifier" then
        is_opaque = true
        break
      end
    end

    local tn = find_child(node, "type_name")
    local name = tn and type_label(tn, source) or ""

    local prefix = is_opaque and "opaque type " or "type "
    local entry = new_entry(SECTION.Type, node, prefix .. name)

    local dc = find_child(node, "data_constructors")
    if dc then
      entry.children = extract_fields_truncated(dc, source, "data_constructor", function(f, src)
        local cn_nodes = f:field("name")
        return (cn_nodes and #cn_nodes > 0) and get_text(cn_nodes[1], src) or "_"
      end)
      entry.child_kind = CHILD_BRIEF
    end

    return entry
  end

  local function extract_named_type(node, source, prefix)
    local tn = find_child(node, "type_name")
    local name = tn and type_label(tn, source) or ""
    return new_entry(SECTION.Type, node, prefix .. name)
  end

  return {
    import_separator = "/",

    is_doc_comment = function(node, _source)
      return node:type() == "statement_comment"
    end,

    is_module_doc = function(node, _source)
      return node:type() == "module_comment"
    end,

    is_attr = function(node)
      return node:type() == "attribute"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "import" then
        local e = extract_import(node, source)
        return e and { e } or {}
      elseif kind == "constant" then
        local e = extract_constant(node, source)
        return e and { e } or {}
      elseif kind == "function" then
        local e = extract_function(node, source, "fn ")
        return e and { e } or {}
      elseif kind == "external_function" then
        local e = extract_function(node, source, "external fn ")
        return e and { e } or {}
      elseif kind == "type_definition" then
        return { extract_type_definition(node, source) }
      elseif kind == "type_alias" then
        return { extract_named_type(node, source, "type ") }
      elseif kind == "external_type" then
        return { extract_named_type(node, source, "external type ") }
      end

      return {}
    end,
  }
end
