-- HCL files are nested blocks of attributes rather than code, so the skeleton
-- is the top-level blocks with their direct members as children. Attributes
-- outside any block (a .tfvars file) render as `name = value` constants.
return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local find_child = U.find_child
  local new_entry = U.new_entry
  local format_skeleton = U.format_skeleton
  local SECTION = U.SECTION

  local LABEL_TRUNCATE = 80

  -- A block's only identifier / string_lit children are its type and labels,
  -- which together read as `resource "aws_instance" "web"`.
  local LABEL_KINDS = { identifier = true, string_lit = true }

  local function block_entry(node, source)
    local parts = {}
    for _, child in ipairs(node:children()) do
      if LABEL_KINDS[child:type()] then
        parts[#parts + 1] = compact_ws(get_text(child, source))
      end
    end
    return new_entry(SECTION.Block, node, truncate(table.concat(parts, " "), LABEL_TRUNCATE))
  end

  -- Members are listed by name only: a top-level block is a section header,
  -- and its values are detail the reader can open the file for.
  local function top_entry(node, source)
    local entry = block_entry(node, source)
    local body = find_child(node, "body")
    for _, child in ipairs(body and body:children() or {}) do
      if child:type() == "block" then
        entry.children[#entry.children + 1] = block_entry(child, source)
      else
        local name = find_child(child, "identifier")
        if name then
          entry.children[#entry.children + 1] = new_entry(SECTION.Block, child, get_text(name, source))
        end
      end
    end
    return entry
  end

  return {
    extract = function(source, root)
      local entries = {}
      local body = find_child(root, "body")
      for _, child in ipairs(body and body:children() or {}) do
        local kind = child:type()
        if kind == "block" then
          entries[#entries + 1] = top_entry(child, source)
        elseif kind == "attribute" then
          local text = truncate(compact_ws(get_text(child, source)), LABEL_TRUNCATE)
          entries[#entries + 1] = new_entry(SECTION.Constant, child, text)
        end
      end
      return format_skeleton(entries, {}, nil, "")
    end,
  }
end
