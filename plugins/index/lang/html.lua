return function(U)
  local get_text = U.get_text
  local line_start = U.line_start
  local line_end = U.line_end
  local format_range = U.format_range
  local find_child = U.find_child

  local STRUCTURAL = {
    html = true,
    head = true,
    body = true,
    header = true,
    footer = true,
    nav = true,
    main = true,
    section = true,
    article = true,
    aside = true,
    div = true,
    form = true,
    table = true,
    thead = true,
    tbody = true,
    tfoot = true,
    tr = true,
    ul = true,
    ol = true,
    dl = true,
    details = true,
    dialog = true,
    template = true,
    slot = true,
    fieldset = true,
  }

  local function attr_value(attr_node)
    local quoted = find_child(attr_node, "quoted_attribute_value")
    if quoted then
      return find_child(quoted, "attribute_value")
    end
    return find_child(attr_node, "attribute_value")
  end

  local function extract_tag_info(tag_node, source)
    local name_node = find_child(tag_node, "tag_name")
    if not name_node then
      return nil
    end
    local tag_name = get_text(name_node, source)
    local id, classes = nil, {}
    for _, child in ipairs(tag_node:children()) do
      if child:type() == "attribute" then
        local aname_node = find_child(child, "attribute_name")
        if aname_node then
          local aname = get_text(aname_node, source)
          local val_node = (aname == "id" or aname == "class") and attr_value(child)
          if val_node then
            local val = get_text(val_node, source)
            if aname == "id" then
              id = val
            else
              for cls in val:gmatch("%S+") do
                classes[#classes + 1] = cls
              end
            end
          end
        end
      end
    end
    return tag_name, id, classes
  end

  local function format_tag(tag_name, id, classes)
    local s = "<" .. tag_name
    if id then
      s = s .. "#" .. id
    end
    for _, cls in ipairs(classes) do
      s = s .. "." .. cls
    end
    return s .. ">"
  end

  local function emit(out, depth, tag_name, id, classes, node)
    local indent = string.rep("  ", depth + 1)
    local lr = format_range(line_start(node), line_end(node))
    out[#out + 1] = indent .. format_tag(tag_name, id, classes) .. " " .. lr
  end

  local function walk(node, source, depth, out)
    local kind = node:type()

    if kind == "element" or kind == "self_closing_tag" then
      local tag_node = node
      if kind == "element" then
        tag_node = find_child(node, "start_tag") or find_child(node, "self_closing_tag")
        if not tag_node then
          return
        end
      end
      local tag_name, id, classes = extract_tag_info(tag_node, source)
      if not tag_name then
        return
      end
      if STRUCTURAL[tag_name] or id then
        emit(out, depth, tag_name, id, classes, node)
        if kind == "element" then
          for _, child in ipairs(node:children()) do
            walk(child, source, depth + 1, out)
          end
        end
      end
    elseif kind == "script_element" or kind == "style_element" then
      local leaf_name = kind == "script_element" and "script" or "style"
      local start_tag = find_child(node, "start_tag")
      local id, classes = nil, {}
      if start_tag then
        _, id, classes = extract_tag_info(start_tag, source)
      end
      emit(out, depth, leaf_name, id, classes or {}, node)
    elseif kind == "document" then
      for _, child in ipairs(node:children()) do
        walk(child, source, depth, out)
      end
    end
  end

  return {
    extract = function(source, root)
      local out = {}
      walk(root, source, 0, out)
      if #out == 0 then
        return ""
      end
      return "structure:\n" .. table.concat(out, "\n") .. "\n"
    end,
  }
end
