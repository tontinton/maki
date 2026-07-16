return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local format_range = U.format_range
  local line_start = U.line_start
  local line_end = U.line_end
  local new_entry = U.new_entry
  local ranged = U.ranged
  local SECTION = U.SECTION

  local SIG_KINDS = {
    function_signature = true,
    getter_signature = true,
    setter_signature = true,
    constructor_signature = true,
    constant_constructor_signature = true,
    factory_constructor_signature = true,
    redirecting_factory_constructor_signature = true,
    operator_signature = true,
  }

  local function type_params(node, source)
    local tp_node = node:field("type_parameters")[1]
    return tp_node and get_text(tp_node, source) or ""
  end

  local function signature_name(sig_node, source)
    local name_nodes = sig_node:field("name")
    if #name_nodes == 0 then
      return nil
    end
    local parts = {}
    for _, n in ipairs(name_nodes) do
      parts[#parts + 1] = get_text(n, source)
    end
    return table.concat(parts)
  end

  local function signature_text(sig_node, source)
    local kind = sig_node:type()
    if kind == "operator_signature" then
      return get_text(sig_node, source)
    end

    local name = signature_name(sig_node, source)
    if not name then
      return nil
    end

    local params_node = find_child(sig_node, "formal_parameter_list")
    local params = params_node and get_text(params_node, source) or "()"
    local tp_node = find_child(sig_node, "type_parameters")
    local tp = tp_node and get_text(tp_node, source) or ""

    if kind == "getter_signature" then
      local ret_node = sig_node:field("return_type")[1]
      local ret = ret_node and (" " .. get_text(ret_node, source)) or ""
      return compact_ws("get " .. name .. ret)
    end

    if kind == "setter_signature" then
      return compact_ws("set " .. name .. params)
    end

    local ret_node = sig_node:field("return_type")[1]
    local ret = ret_node and get_text(ret_node, source)
    if ret == "set" then
      return compact_ws("set " .. name .. params)
    elseif ret == "get" and params == "()" then
      return compact_ws("get " .. name)
    end
    local ret_s = ret and (" " .. ret) or ""
    return compact_ws(name .. tp .. params .. ret_s)
  end

  local function find_signature(node)
    if node:type() == "method_declaration" then
      local method_sig = node:field("signature")[1]
      if method_sig then
        for _, child in ipairs(method_sig:children()) do
          if SIG_KINDS[child:type()] then
            return child
          end
        end
      end
      return nil
    end

    for _, child in ipairs(node:children()) do
      if SIG_KINDS[child:type()] then
        return child
      end
    end
    return nil
  end

  local FIELD_LIST_KINDS = {
    initialized_identifier_list = "initialized_identifier",
    identifier_list = "identifier",
    static_final_declaration_list = "static_final_declaration",
  }

  local function field_text(id_node, source, type_node)
    local name
    if id_node:type() == "identifier" then
      name = get_text(id_node, source)
    else
      local name_node = id_node:field("name")[1]
      if not name_node then
        return nil
      end
      name = get_text(name_node, source)
    end
    if type_node then
      return name .. " " .. get_text(type_node, source)
    end
    return name
  end

  local function add_field(out, id_node, source, type_node, range_node)
    local text = field_text(id_node, source, type_node)
    if text then
      local lr = format_range(line_start(range_node), line_end(range_node))
      out[#out + 1] = ranged(text, lr)
    end
  end

  local function extract_field_like(node, source, out)
    local type_node = find_child(node, "type")
    for _, child in ipairs(node:children()) do
      local ckind = child:type()
      local list_kind = FIELD_LIST_KINDS[ckind]
      if list_kind then
        for _, id in ipairs(child:children()) do
          if id:type() == list_kind then
            add_field(out, id, source, type_node, id)
          end
        end
      elseif ckind == "initialized_identifier" or ckind == "static_final_declaration" or ckind == "identifier" then
        add_field(out, child, source, type_node, child)
      end
    end
  end

  local function unwrap_class_member(member)
    if member:type() ~= "class_member" then
      return member
    end
    for _, child in ipairs(member:children()) do
      local ckind = child:type()
      if ckind == "method_declaration" or ckind == "declaration" then
        return child
      end
    end
    return nil
  end

  local function extract_member(member, source)
    local actual = unwrap_class_member(member)
    if not actual then
      return {}
    end
    local kind = actual:type()
    if kind == "method_declaration" or kind == "declaration" then
      local sig = find_signature(actual)
      if sig then
        local text = signature_text(sig, source)
        if text then
          local lr = format_range(line_start(actual), line_end(actual))
          return { ranged(text, lr) }
        end
      end
      if kind == "declaration" then
        local fields = {}
        extract_field_like(actual, source, fields)
        return fields
      end
    end
    return {}
  end

  local function extract_body_members(body_node, source)
    local members = {}
    for _, child in ipairs(body_node:children()) do
      for _, m in ipairs(extract_member(child, source)) do
        members[#members + 1] = m
      end
    end
    return members
  end

  local function extract_classlike(node, source, prefix)
    local name_node = node:field("name")[1]
    if not name_node then
      return nil
    end
    local name = get_text(name_node, source)
    local tp = type_params(node, source)
    local body_node = node:field("body")[1]
    local entry = new_entry(SECTION.Class, node, prefix .. " " .. name .. tp)
    if body_node then
      entry.children = extract_body_members(body_node, source)
    end
    return entry
  end

  local function extract_function(node, source)
    local sig_node = node:field("signature")[1]
    if not sig_node then
      return nil
    end
    local text = signature_text(sig_node, source)
    if not text then
      return nil
    end
    return new_entry(SECTION.Function, node, text)
  end

  return {
    import_separator = ".",
    is_doc_comment = function(node, source)
      return node:type() == "comment" and get_text(node, source):sub(1, 3) == "///"
    end,

    extract_nodes = function(node, source, _attrs)
      local kind = node:type()

      if kind == "class_declaration" then
        local e = extract_classlike(node, source, "class")
        return e and { e } or {}
      elseif kind == "mixin_declaration" then
        local e = extract_classlike(node, source, "mixin")
        return e and { e } or {}
      elseif kind == "extension_type_declaration" then
        local e = extract_classlike(node, source, "extension type")
        return e and { e } or {}
      elseif kind == "extension_declaration" then
        local body_node = node:field("body")[1]
        if body_node then
          local name_node = node:field("name")[1]
          local name = name_node and get_text(name_node, source) or "_"
          local entry = new_entry(SECTION.Type, node, "extension " .. name)
          entry.children = extract_body_members(body_node, source)
          return { entry }
        end
        return {}
      elseif kind == "enum_declaration" then
        local name_node = node:field("name")[1]
        if not name_node then
          return {}
        end
        local name = get_text(name_node, source)
        local tp = type_params(node, source)
        return { new_entry(SECTION.Type, node, "enum " .. name .. tp) }
      elseif kind == "function_declaration" or kind == "external_function_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}
      elseif kind == "getter_declaration" or kind == "external_getter_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}
      elseif kind == "setter_declaration" or kind == "external_setter_declaration" then
        local e = extract_function(node, source)
        return e and { e } or {}
      end

      return {}
    end,
  }
end
