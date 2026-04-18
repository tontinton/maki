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
  local SECTION = U.SECTION

  local function extract_require(node, source, sep)
    local method_node = node:field("method")[1]
    if not method_node then return nil end
    local method = get_text(method_node, source)
    if method ~= "require" and method ~= "require_relative" then return nil end
    local args = find_child(node, "argument_list")
    if not args then return nil end
    local str_node = find_child(args, "string")
    if not str_node then return nil end
    local raw = get_text(str_node, source):match("^['\"](.+)['\"]$")
    if not raw then return nil end
    local path = {}
    for part in raw:gmatch("[^" .. sep .. "]+") do path[#path + 1] = part end
    return new_import_entry(node, { path })
  end

  local function method_sig(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local params_node = node:field("parameters")[1]
    local params = params_node and get_text(params_node, source) or "()"
    return compact_ws(name .. params)
  end

  local function extract_class(node, source)
    local name_node = node:field("name")[1]
    if not name_node then return nil end
    local name = get_text(name_node, source)
    local super_node = node:field("superclass")[1]
    local text = name
    if super_node then
      local st = get_text(super_node, source):gsub("^<", ""):match("^%s*(.-)%s*$")
      text = name .. " < " .. st
    end
    local body = node:field("body")[1]
    local methods = {}
    if body then
      for _, child in ipairs(body:children()) do
        local ckind = child:type()
        local sig = nil
        if ckind == "method" then
          sig = method_sig(child, source)
        elseif ckind == "singleton_method" then
          local inner = method_sig(child, source)
          sig = inner and ("self." .. inner) or nil
        end
        if sig then
          methods[#methods + 1] = sig .. " " .. format_range(line_start(child), line_end(child))
        end
      end
    end
    local entry = new_entry(SECTION.Class, node, text)
    entry.children = methods
    return entry
  end

  local function extract_module(node, source, extract_nodes_fn)
    local name_node = node:field("name")[1]
    if not name_node then return {} end
    local entries = { new_entry(SECTION.Module, node, get_text(name_node, source)) }
    local body = node:field("body")[1]
    if body then
      for _, child in ipairs(body:children()) do
        local extracted = extract_nodes_fn(child, source, {})
        for _, e in ipairs(extracted) do entries[#entries + 1] = e end
      end
    end
    return entries
  end

  local function extract_nodes(node, source, _attrs)
    local kind = node:type()
    local sep = "/"

    if kind == "call" then
      local imp = extract_require(node, source, sep)
      return imp and { imp } or {}

    elseif kind == "class" then
      local e = extract_class(node, source)
      return e and { e } or {}

    elseif kind == "module" then
      return extract_module(node, source, extract_nodes)

    elseif kind == "method" then
      local name_node = node:field("name")[1]
      if not name_node then return {} end
      local sig = method_sig(node, source)
      return sig and { new_entry(SECTION.Function, node, sig) } or {}

    elseif kind == "singleton_method" then
      local inner = method_sig(node, source)
      if not inner then return {} end
      return { new_entry(SECTION.Function, node, "self." .. inner) }

    elseif kind == "assignment" then
      local left_node = node:field("left")[1]
      if not left_node then return {} end
      local name = get_text(left_node, source)
      if not name:match("^[A-Z]") then return {} end
      local right_node = node:field("right")[1]
      local val_str = right_node and (" = " .. truncate(get_text(right_node, source), 60)) or ""
      return { new_entry(SECTION.Constant, node, name .. val_str) }
    end

    return {}
  end

  return {
    import_separator = "/",
    is_doc_comment = function(node, _source) return node:type() == "comment" end,
    extract_nodes = extract_nodes,
  }
end
