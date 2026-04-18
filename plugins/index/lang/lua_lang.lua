return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION

  local function require_module(node, source)
    local name_node = node:field("name")[1]
    if not name_node or get_text(name_node, source) ~= "require" then return nil end
    local args = node:field("arguments")[1]
    if not args then return nil end
    for _, child in ipairs(args:children()) do
      local t = child:type()
      if t == "string" then
        local s = get_text(child, source)
        return s:match("^['\"](.+)['\"]$") or s
      end
    end
    return nil
  end

  local function import_from_require(node, source)
    local mod = require_module(node, source)
    if not mod then return nil end
    local path = {}
    for part in mod:gmatch("[^%.]+") do path[#path + 1] = part end
    return new_import_entry(node, { path })
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, source)
      return node:type() == "comment" and get_text(node, source):sub(1, 3) == "---"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "function_declaration" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local params_node = node:field("parameters")[1]
        local params = params_node and get_text(params_node, source) or "()"
        return { new_entry(SECTION.Function, node, compact_ws(get_text(name_node, source) .. params)) }

      elseif kind == "variable_declaration" then
        local assign = find_child(node, "assignment_statement")
        if not assign then return {} end
        local var_list = find_child(assign, "variable_list")
        local expr_list = find_child(assign, "expression_list")
        if not var_list or not expr_list then return {} end

        local results = {}
        local expr_children = {}
        for _, child in ipairs(expr_list:children()) do
          if child:type() ~= "," then
            expr_children[#expr_children + 1] = child
          end
        end

        for i, expr in ipairs(expr_children) do
          if expr:type() == "function_call" then
            local imp = import_from_require(expr, source)
            if imp then results[#results + 1] = imp end
          end
        end

        if #results > 0 then return results end

        local var_children = {}
        for _, child in ipairs(var_list:children()) do
          if child:type() ~= "," then var_children[#var_children + 1] = child end
        end

        if #var_children == 1 then
          local name = get_text(var_children[1], source)
          if name:match("^[A-Z_]+$") then
            local val_node = expr_children[1]
            local val_str = val_node and (" = " .. U.truncate(get_text(val_node, source), 60)) or ""
            return { new_entry(SECTION.Constant, node, name .. val_str) }
          end
        end
        return {}

      elseif kind == "function_call" then
        local imp = import_from_require(node, source)
        return imp and { imp } or {}
      end

      return {}
    end,
  }
end
