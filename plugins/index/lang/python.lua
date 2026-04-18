return function(U)
  local get_text = U.get_text
  local line_start = U.line_start
  local line_end = U.line_end
  local format_range = U.format_range
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION

  local function is_module_doc(node, source)
    if node:type() ~= "expression_statement" then return false end
    local first = node:child(0)
    if not first then return false end
    return first:type() == "string" and get_text(first, source):sub(1, 3) == '"""'
  end

  local function extract_import(node, source)
    local text = get_text(node, source)
    local cleaned = text:match("^import (.+)") or text:match("^from (.+)") or text
    cleaned = cleaned:match("^%s*(.-)%s*$")
    local base, names = cleaned:match("^(.+) import (.+)$")
    local paths
    if base then
      paths = {}
      local base_parts = {}
      for part in base:gmatch("[^%.]+") do
        base_parts[#base_parts + 1] = part
      end
      for name in names:gmatch("[^,]+") do
        local path = {}
        for _, p in ipairs(base_parts) do path[#path + 1] = p end
        path[#path + 1] = name:match("^%s*(.-)%s*$")
        paths[#paths + 1] = path
      end
    else
      paths = {}
      local path = {}
      for part in cleaned:gmatch("[^%.]+") do
        path[#path + 1] = part
      end
      paths[1] = path
    end
    return new_import_entry(node, paths)
  end

  local function extract_class(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local body_node = node:field("body")[1]
    if not body_node then return nil end
    local methods = {}
    for _, child in ipairs(body_node:children()) do
      local fn_node = nil
      local is_decorated = false
      if child:type() == "decorated_definition" then
        fn_node = find_child(child, "function_definition")
        is_decorated = true
      elseif child:type() == "function_definition" then
        fn_node = child
      end
      if fn_node then
        local fn_name_node = fn_node:field("name")[1]
        local fn_name = fn_name_node and get_text(fn_name_node, source) or "_"
        local params_node = fn_node:field("parameters")[1]
        local params = params_node and get_text(params_node, source) or "()"
        local ret_node = fn_node:field("return_type")[1]
        local ret_str = ret_node and (" -> " .. get_text(ret_node, source)) or ""
        local lr = format_range(line_start(fn_node), line_end(fn_node))
        if is_decorated then
          for _, dec in ipairs(child:children()) do
            if dec:type() == "decorator" then
              methods[#methods + 1] = get_text(dec, source)
            end
          end
        end
        methods[#methods + 1] = compact_ws(fn_name .. params .. ret_str .. " " .. lr)
      end
    end
    local entry = new_entry(SECTION.Class, node, name)
    entry.children = methods
    return entry
  end

  local function extract_function(node, source)
    local actual = node
    if node:type() == "decorated_definition" then
      actual = find_child(node, "function_definition")
      if not actual then return nil end
    end
    local name_node = actual:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = actual:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "()"
    local ret_node = actual:field("return_type")[1]
    local ret_str = ret_node and (" -> " .. get_text(ret_node, source)) or ""
    return new_entry(SECTION.Function, node, compact_ws(name .. params .. ret_str))
  end

  local function extract_assignment(node, source)
    local left = node:child(0)
    if not left then return nil end
    local name = get_text(left, source)
    if not name:match("^[A-Z_]+$") then return nil end
    local found_eq = false
    local val = nil
    for i = 0, node:child_count() - 1 do
      local c = node:child(i)
      if found_eq then
        val = c
        break
      end
      if get_text(c, source) == "=" then
        found_eq = true
      end
    end
    local val_str = val and (" = " .. truncate(get_text(val, source), 60)) or ""
    return new_entry(SECTION.Constant, node, name .. val_str)
  end

  local function extract_nodes(node, source, _attrs)
    local kind = node:type()
    if kind == "import_statement" or kind == "import_from_statement" then
      return { extract_import(node, source) }
    elseif kind == "class_definition" then
      local e = extract_class(node, source)
      return e and { e } or {}
    elseif kind == "function_definition" then
      local e = extract_function(node, source)
      return e and { e } or {}
    elseif kind == "decorated_definition" then
      local inner = find_child(node, "class_definition")
        or find_child(node, "function_definition")
      if inner and inner:type() == "class_definition" then
        local e = extract_class(inner, source)
        if e then e.line_start = line_start(node) end
        return e and { e } or {}
      elseif inner then
        local e = extract_function(node, source)
        return e and { e } or {}
      end
      return {}
    elseif kind == "expression_statement" then
      local first = node:child(0)
      if first and first:type() == "assignment" then
        local e = extract_assignment(first, source)
        return e and { e } or {}
      end
      return {}
    end
    return {}
  end

  return {
    extract_nodes = extract_nodes,
    import_separator = ".",
    is_module_doc = is_module_doc,
  }
end
