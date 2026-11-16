return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION

  local PARAM_TRUNCATE = 80
  local MEMBER_TRUNCATE = 60
  local SIGNATURE_TRUNCATE = 100

  -- Nodes whose children are indexed but which are not themselves an entry.
  -- Packages wrap each item in `package_item`, classes in `class_item` plus
  -- `class_property` or `class_method`; modules and interfaces hold theirs
  -- directly. `member_lines` walks only as far as `class_property`, because a
  -- method is not a field.
  local TRANSPARENT = {
    package_item = true,
    class_item = true,
    class_property = true,
    class_method = true,
  }

  -- A labelled procedural block sits a few wrappers down from its construct.
  local PROCEDURAL_CONSTRUCT = {
    initial_construct = true,
    always_construct = true,
    final_construct = true,
  }

  local function field_text(node, field, source)
    local value = node:field(field)[1]
    return value and get_text(value, source) or nil
  end

  local function one_line(node, source)
    return compact_ws(get_text(node, source))
  end

  -- The grammar spells a type three ways (`data_type`, `data_type_or_void`,
  -- `data_type_or_implicit`) plus `variable_port_type` on ports; every call
  -- site that asks "is this the type?" means all four.
  local function is_type_node(kind)
    return kind:find("^data_type") ~= nil or kind == "variable_port_type"
  end

  -- Modules and interfaces name themselves on a `*_header` child, packages
  -- and classes on the declaration itself.
  local function declaration_name(node, source)
    local name = field_text(node, "name", source)
    if name then
      return name
    end
    for _, child in ipairs(node:named_children()) do
      if child:type():find("_header", 1, true) then
        return field_text(child, "name", source)
      end
    end
    return nil
  end

  -- The declared names hang off a hidden assignment list, so they are reached
  -- by walking for the assignment leaves. A type holds no declarators, so the
  -- walk skips it rather than descending.
  local function declared_names(node, source)
    local names = {}
    local stack = { node }
    while #stack > 0 do
      local current = table.remove(stack)
      if current:type() == "variable_decl_assignment" or current:type() == "param_assignment" then
        local name = field_text(current, "name", source)
          or current:named_child(0) and one_line(current:named_child(0), source)
        if name then
          names[#names + 1] = name
        end
      elseif not is_type_node(current:type()) then
        for _, child in ipairs(current:named_children()) do
          stack[#stack + 1] = child
        end
      end
    end
    return names
  end

  local function type_text(node, source)
    for _, child in ipairs(node:named_children()) do
      if is_type_node(child:type()) then
        return truncate(one_line(child, source), MEMBER_TRUNCATE)
      end
    end
    return nil
  end

  -- The property chain a field hides behind; a method sits under
  -- `class_method` instead and is deliberately not followed here.
  local MEMBER_WRAPPER = { class_item = true, class_property = true }

  local function member_lines(node, source)
    local lines = {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if MEMBER_WRAPPER[kind] then
        for _, line in ipairs(member_lines(child, source)) do
          lines[#lines + 1] = line
        end
      elseif kind == "data_declaration" or kind == "struct_union_member" then
        local names = declared_names(child, source)
        if #names > 0 then
          local decl_type = type_text(child, source)
          lines[#lines + 1] =
            truncate((decl_type and (decl_type .. " ") or "") .. table.concat(names, ", "), MEMBER_TRUNCATE)
        end
      end
    end
    return lines
  end

  local function with_members(entry, node, source)
    local lines = member_lines(node, source)
    for i, line in ipairs(lines) do
      if i > U.FIELD_TRUNCATE_THRESHOLD then
        entry.children[#entry.children + 1] = U.truncated_msg(#lines)
        break
      end
      entry.children[#entry.children + 1] = line
    end
    return entry
  end

  local function extract_imports(node, source)
    local entries = {}
    for _, item in ipairs(node:named_children()) do
      if item:type() == "package_import_item" then
        local idents = {}
        for _, ident in ipairs(item:named_children()) do
          idents[#idents + 1] = one_line(ident, source)
        end
        local entry = #idents > 0 and new_import_entry(item, { idents })
        if entry then
          entries[#entries + 1] = entry
        end
      end
    end
    return entries
  end

  -- `automatic`/`static` parse as a `lifetime` sibling, so the body is not
  -- necessarily the first named child.
  local function body_declaration(node, kind)
    for _, child in ipairs(node:named_children()) do
      if child:type() == kind then
        return child
      end
    end
    return nil
  end

  local function signature_entry(node, source)
    local name = field_text(node, "name", source)
    if not name then
      return nil
    end
    local ports = body_declaration(node, "tf_port_list")
    local params = ports and truncate("(" .. one_line(ports, source) .. ")", SIGNATURE_TRUNCATE) or "()"
    return new_entry(SECTION.Function, node, name .. params)
  end

  local function extract_subprogram(node, source, body_kind)
    local body = body_declaration(node, body_kind)
    return body and signature_entry(body, source) or nil
  end

  local extract_function = function(node, source)
    return extract_subprogram(node, source, "function_body_declaration")
  end

  local extract_task = function(node, source)
    return extract_subprogram(node, source, "task_body_declaration")
  end

  -- `enum logic [1:0] { IDLE, RUN }`; the literals are listed separately, so
  -- only the head before the brace is kept here.
  local function enum_head(data_type, source)
    local text = one_line(data_type, source)
    return (text:gsub("%s*{.*$", ""))
  end

  local function enum_literals(data_type, source)
    local literals = {}
    for _, child in ipairs(data_type:named_children()) do
      if child:type() == "enum_name_declaration" then
        literals[#literals + 1] = one_line(child, source)
      end
    end
    return literals
  end

  -- An enum body lists its literals, a struct or union body its members. Both
  -- sit as siblings of the shape marker under the `data_type`, so the walk
  -- starts there in either case.
  local function typedef_body(data_type, source)
    for _, child in ipairs(data_type:named_children()) do
      local kind = child:type()
      if kind == "enum_name_declaration" then
        return enum_literals(data_type, source)
      elseif kind == "struct_union" then
        return member_lines(data_type, source)
      end
    end
    return {}
  end

  -- The declaration is `typedef <data_type> <name>`, so the shape is the type.
  local function extract_typedef(node, source)
    local name = field_text(node, "type_name", source)
    if not name then
      return nil
    end
    local data_type = nil
    for _, child in ipairs(node:named_children()) do
      if child:type() == "data_type" then
        data_type = child
        break
      end
    end
    local text = "typedef " .. name .. (data_type and (" = " .. enum_head(data_type, source)) or "")
    local entry = new_entry(SECTION.Type, node, truncate(text, PARAM_TRUNCATE))
    if not data_type then
      return entry
    end
    entry.children = typedef_body(data_type, source)
    return entry
  end

  -- `class B extends A` carries the parent in a `class_type` child; interfaces
  -- and packages have nothing comparable.
  local function extends_suffix(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "class_type" then
        return " extends " .. one_line(child, source)
      end
    end
    return ""
  end

  local function extract_class(node, source)
    local name = declaration_name(node, source)
    if not name then
      return nil
    end
    local label = "class " .. name .. extends_suffix(node, source)
    return with_members(new_entry(SECTION.Class, node, label), node, source)
  end

  -- Modports name the directions a bus port is viewed through, which is the
  -- interface's real contract; the members alone do not show them.
  local function extract_modport(node, source)
    local entries = {}
    for _, item in ipairs(node:named_children()) do
      if item:type() == "modport_item" then
        local name = item:named_child(0)
        if name then
          entries[#entries + 1] =
            new_entry(SECTION.Type, item, truncate("modport " .. one_line(item, source), PARAM_TRUNCATE))
        end
      end
    end
    return entries
  end

  -- Generate blocks name themselves through a field, a `seq_block` through a
  -- leading identifier.
  local function labelled(node, source, keyword)
    local name = field_text(node, "name", source)
    if not name then
      local first = node:named_child(0)
      if first and first:type() == "simple_identifier" then
        name = get_text(first, source)
      end
    end
    return name and (keyword .. " " .. name) or keyword
  end

  -- `extern function`/`extern task` outside a class (the idiomatic way to
  -- declare an out-of-body prototype) fails to parse in tree-sitter-systemverilog
  -- 0.4.1: the error recovery drops earlier declarations from the tree and
  -- shifts later line ranges. Items after the `extern` still extract, but the
  -- file is not fully indexed until the grammar handles it.
  -- `extern` and `pure virtual` members carry no body, only the head.
  local PROTOTYPE = { function_prototype = true, task_prototype = true }

  local function dispatch(node, source)
    local kind = node:type()
    if kind == "function_declaration" then
      local entry = extract_function(node, source)
      return entry and { entry } or {}
    elseif kind == "task_declaration" then
      local entry = extract_task(node, source)
      return entry and { entry } or {}
    elseif PROTOTYPE[kind] then
      local entry = signature_entry(node, source)
      return entry and { entry } or {}
    elseif kind == "modport_declaration" then
      return extract_modport(node, source)
    elseif kind == "generate_block" then
      return { with_members(new_entry(SECTION.Block, node, labelled(node, source, "generate")), node, source) }
    elseif kind == "seq_block" then
      return { with_members(new_entry(SECTION.Block, node, labelled(node, source, "block")), node, source) }
    end
    local name = declaration_name(node, source)
    if not name then
      return {}
    end
    if kind == "module_declaration" then
      return { new_entry(SECTION.Class, node, "module " .. name) }
    elseif kind == "interface_declaration" then
      return { with_members(new_entry(SECTION.Type, node, "interface " .. name), node, source) }
    elseif kind == "package_declaration" then
      return { new_entry(SECTION.Module, node, "package " .. name) }
    elseif kind == "class_declaration" then
      local entry = extract_class(node, source)
      return entry and { entry } or {}
    end
    return {}
  end

  local extract_nodes

  -- `default_extract` only walks the root's children, so a container that
  -- wants its own items indexed has to return them from its own node.
  local function inner_entries(node, source)
    local entries = {}
    local stack = { node }
    while #stack > 0 do
      local current = table.remove(stack)
      for _, child in ipairs(current:named_children()) do
        -- The module and interface headers hold the parameter ports.
        if child:type():find("_header", 1, true) then
          stack[#stack + 1] = child
        else
          for _, entry in ipairs(extract_nodes(child, source)) do
            entries[#entries + 1] = entry
          end
        end
      end
    end
    return entries
  end

  extract_nodes = function(node, source, _attrs)
    local kind = node:type()
    if TRANSPARENT[kind] then
      return inner_entries(node, source)
    elseif kind == "parameter_port_list" or kind == "parameter_port_declaration" then
      local entries = {}
      for _, child in ipairs(node:named_children()) do
        for _, entry in ipairs(extract_nodes(child, source)) do
          entries[#entries + 1] = entry
        end
      end
      return entries
    elseif
      kind == "generate_region"
      or kind == "loop_generate_construct"
      or kind == "conditional_generate_construct"
    then
      return inner_entries(node, source)
    elseif PROCEDURAL_CONSTRUCT[kind] or kind:find("^always") or kind:find("^statement") then
      return inner_entries(node, source)
    elseif
      kind == "module_declaration"
      or kind == "interface_declaration"
      or kind == "package_declaration"
      or kind == "class_declaration"
    then
      local entries = dispatch(node, source)
      for _, entry in ipairs(inner_entries(node, source)) do
        entries[#entries + 1] = entry
      end
      return entries
    elseif kind == "package_import_declaration" then
      return extract_imports(node, source)
    elseif kind == "parameter_declaration" or kind == "local_parameter_declaration" then
      local keyword = kind == "local_parameter_declaration" and "localparam " or ""
      local entries = {}
      for _, name in ipairs(declared_names(node, source)) do
        entries[#entries + 1] = new_entry(SECTION.Constant, node, truncate(keyword .. name, PARAM_TRUNCATE))
      end
      return entries
    elseif kind == "data_declaration" then
      for _, child in ipairs(node:named_children()) do
        if child:type() == "type_declaration" then
          local entry = extract_typedef(child, source)
          return entry and { entry } or {}
        end
      end
      local decl_type = type_text(node, source)
      local names = declared_names(node, source)
      if #names == 0 then
        return {}
      end
      return {
        new_entry(
          SECTION.Constant,
          node,
          truncate((decl_type and (decl_type .. " ") or "") .. table.concat(names, ", "), MEMBER_TRUNCATE)
        ),
      }
    end
    return dispatch(node, source)
  end

  return {
    import_separator = "::",

    is_doc_comment = function(node, _source)
      return node:type() == "comment"
    end,

    extract_nodes = extract_nodes,
  }
end
