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

  local DEF = "def"
  local DEFP = "defp"
  local IMPORT_KEYWORDS = { alias = true, import = true, require = true, use = true }
  local DOC_ATTRS = { doc = true, moduledoc = true, typedoc = true, spec = true }

  local function call_target(node, source)
    if node:type() ~= "call" then return nil end
    local t = node:field("target")[1]
    return t and get_text(t, source) or nil
  end

  local function first_arg(node, source)
    local args = find_child(node, "arguments")
    if not args then return nil end
    for _, child in ipairs(args:children()) do
      local ck = child:type()
      if ck ~= "," and ck ~= "(" and ck ~= ")" then
        return child
      end
    end
    return nil
  end

  local function split_dot(text)
    local parts = {}
    for p in text:gmatch("[^%.]+") do
      parts[#parts + 1] = p
    end
    return parts
  end

  local function extract_import(node, source)
    local target = call_target(node, source)
    if not target or not IMPORT_KEYWORDS[target] then return nil end
    local arg = first_arg(node, source)
    if not arg then return nil end
    local arg_text = get_text(arg, source)
    local paths = { split_dot(arg_text) }
    return new_import_entry(node, paths, target)
  end

  local function def_sig(node, source)
    local args = find_child(node, "arguments")
    if not args then return nil end
    local arg = first_arg(node, source)
    if not arg then return nil end
    local ck = arg:type()
    if ck == "call" or ck == "identifier" or ck == "binary_operator" then
      return get_text(arg, source)
    end
    return nil
  end

  local function is_def_call(node, source)
    local target = call_target(node, source)
    return target == DEF or target == DEFP
  end

  local function extract_standalone_fn(node, source)
    if not is_def_call(node, source) then return nil end
    local sig = def_sig(node, source)
    if not sig then return nil end
    local target = call_target(node, source)
    local prefix = target == DEFP and "defp " or "def "
    return new_entry(SECTION.Function, node, compact_ws(prefix .. sig))
  end

  local function attr_name(node, source)
    if node:type() ~= "unary_operator" then return nil end
    local op = node:field("operator")[1]
    if not op or get_text(op, source) ~= "@" then return nil end
    local operand = node:field("operand")[1]
    if not operand then return nil end
    local ok = operand:type()
    if ok == "identifier" or ok == "alias" then
      return get_text(operand, source)
    elseif ok == "call" then
      return call_target(operand, source)
    end
    return nil
  end

  local function extract_module_attr(node, source)
    local name = attr_name(node, source)
    if not name then return nil end
    if DOC_ATTRS[name] then return nil end
    if not name:match("^[A-Z]") then return nil end
    return new_entry(SECTION.Constant, node, "@" .. name)
  end

  local function extract_module(node, source)
    local target = call_target(node, source)
    if target ~= "defmodule" then return nil end
    local arg = first_arg(node, source)
    if not arg then return nil end
    local name = get_text(arg, source)

    local do_block = find_child(node, "do_block")
    if not do_block then
      return new_entry(SECTION.Class, node, "defmodule " .. name)
    end

    local imports = {}
    local methods = {}

    for _, child in ipairs(do_block:children()) do
      local ck = child:type()
      if ck == "call" then
        local imp = extract_import(child, source)
        if imp then
          imports[#imports + 1] = imp
        elseif is_def_call(child, source) then
          local sig = def_sig(child, source)
          if sig then
            local t = call_target(child, source)
            local prefix = t == DEFP and "defp " or "def "
            local lr = format_range(line_start(child), line_end(child))
            methods[#methods + 1] = compact_ws(prefix .. sig) .. " " .. lr
          end
        end
      end
    end

    local entry = new_entry(SECTION.Class, node, "defmodule " .. name)
    entry.children = methods
    return entry, imports
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, source)
      local ck = node:type()
      if ck == "comment" then return true end
      if ck == "unary_operator" then
        local name = attr_name(node, source)
        return name ~= nil and DOC_ATTRS[name] == true
      end
      return false
    end,

    extract_nodes = function(node, source, _attrs)
      local ck = node:type()
      if ck == "call" then
        local imp = extract_import(node, source)
        if imp then return { imp } end
        local mod_entry, mod_imports = extract_module(node, source)
        if mod_entry then
          local result = {}
          if mod_imports then
            for _, mi in ipairs(mod_imports) do
              result[#result + 1] = mi
            end
          end
          result[#result + 1] = mod_entry
          return result
        end
        local fn_entry = extract_standalone_fn(node, source)
        if fn_entry then return { fn_entry } end
      elseif ck == "unary_operator" then
        local e = extract_module_attr(node, source)
        if e then return { e } end
      end
      return {}
    end,
  }
end
