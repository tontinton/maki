return function(U)
  local get_text = U.get_text
  local new_entry = U.new_entry
  local truncate = U.truncate
  local SECTION = U.SECTION

  return {
    import_separator = "/",

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "function_definition" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        return { new_entry(SECTION.Function, node, get_text(name_node, source) .. "()") }

      elseif kind == "variable_assignment" then
        local name_node = node:field("name")[1]
        if not name_node then return {} end
        local name = get_text(name_node, source)
        if not name:match("^[A-Z_]+$") then return {} end
        local val_node = node:field("value")[1]
        local val_str = val_node and (" = " .. truncate(get_text(val_node, source), 60)) or ""
        return { new_entry(SECTION.Constant, node, name .. val_str) }
      end

      return {}
    end,
  }
end
