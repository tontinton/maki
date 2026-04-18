local FIELD_TRUNCATE_THRESHOLD = 8
local LINE_WRAP_THRESHOLD = 120
local MAX_INT = math.maxinteger or 2^53

local EXT_TO_LANG = {
  rs = "rust",
  py = "python", pyi = "python",
  ts = "typescript", tsx = "typescript",
  js = "javascript", jsx = "javascript", mjs = "javascript", cjs = "javascript",
}

local function get_text(node, source)
  return maki.treesitter.get_node_text(node, source)
end

local function line_start(node)
  local row = node:start()
  return row + 1
end

local function line_end(node)
  local row = node:end_()
  return row + 1
end

local function format_range(s, e)
  if s == e then return "[" .. s .. "]" end
  return "[" .. s .. "-" .. e .. "]"
end

local function find_child(node, kind)
  for _, child in ipairs(node:children()) do
    if child:type() == kind then return child end
  end
  return nil
end

local function compact_ws(s)
  return (s:gsub("%s+", " "))
end

local function truncate(s, max)
  if #s <= max then return s end
  local boundary = max - 11
  if boundary < 0 then boundary = 0 end
  return s:sub(1, boundary) .. "[truncated]"
end

local function truncated_msg(total)
  return "[" .. (total - FIELD_TRUNCATE_THRESHOLD) .. " more truncated]"
end

local function wrap_csv(items, indent)
  local lines = {}
  local current = indent
  for i, item in ipairs(items) do
    local addition
    if i == 1 then
      addition = item
    else
      addition = ", " .. item
    end
    if i > 1 and #current + #addition > LINE_WRAP_THRESHOLD then
      lines[#lines + 1] = current
      current = indent .. item
    else
      current = current .. addition
    end
  end
  if current:match("%S") then
    lines[#lines + 1] = current
  end
  return lines
end

local function new_trie()
  return { children = {}, is_leaf = false, _keys = {} }
end

local function trie_insert(trie, segments)
  local node = trie
  for _, seg in ipairs(segments) do
    if not node.children[seg] then
      node.children[seg] = new_trie()
      node._keys[#node._keys + 1] = seg
    end
    node = node.children[seg]
  end
  node.is_leaf = true
end

local function sorted_keys(trie)
  local keys = {}
  local seen = {}
  for _, k in ipairs(trie._keys) do
    if not seen[k] then
      seen[k] = true
      keys[#keys + 1] = k
    end
  end
  table.sort(keys)
  return keys
end

local render_children

local function render_node(seg, node, sep)
  if not next(node.children) then
    return { seg }
  end

  local rendered = render_children(node, sep)

  if node.is_leaf then
    local out = { seg }
    for _, item in ipairs(rendered) do
      out[#out + 1] = seg .. sep .. item
    end
    return out
  end

  if #rendered == 1 then
    return { seg .. sep .. rendered[1] }
  else
    return { seg .. sep .. "{" .. table.concat(rendered, ", ") .. "}" }
  end
end

render_children = function(trie, sep)
  local result = {}
  local keys = sorted_keys(trie)
  for _, seg in ipairs(keys) do
    local node = trie.children[seg]
    for _, line in ipairs(render_node(seg, node, sep)) do
      result[#result + 1] = line
    end
  end
  return result
end

-- Brace-aware separator search so nested imports like std::{fs, net::{TcpStream}} split correctly.
local function find_sep(text, sep)
  local depth = 0
  local sep_len = #sep
  for i = 1, #text do
    local c = text:sub(i, i)
    if c == "{" then
      depth = depth + 1
    elseif c == "}" then
      if depth > 0 then depth = depth - 1 end
    elseif depth == 0 and text:sub(i, i + sep_len - 1) == sep then
      return i
    end
  end
  return nil
end

local function split_top_level(text, delim)
  local results = {}
  local depth = 0
  local start = 1
  for i = 1, #text do
    local c = text:byte(i)
    if c == 123 then -- {
      depth = depth + 1
    elseif c == 125 then -- }
      if depth > 0 then depth = depth - 1 end
    elseif c == delim:byte(1) and depth == 0 then
      local part = text:sub(start, i - 1)
      part = part:match("^%s*(.-)%s*$")
      results[#results + 1] = part
      start = i + 1
    end
  end
  local last = text:sub(start)
  last = last:match("^%s*(.-)%s*$")
  if #last > 0 then
    results[#results + 1] = last
  end
  return results
end

local function expand_import(text, sep)
  local results = {}
  local stack = { { {}, text:match("^%s*(.-)%s*$") } }

  while #stack > 0 do
    local top = table.remove(stack)
    local prefix, remaining = top[1], top[2]

    if #remaining == 0 then
      if #prefix > 0 then
        results[#results + 1] = prefix
      end
    else
      local pos = find_sep(remaining, sep)
      if not pos then
        local path = {}
        for _, p in ipairs(prefix) do path[#path + 1] = p end
        path[#path + 1] = remaining
        results[#results + 1] = path
      else
        local segment = remaining:sub(1, pos - 1)
        local rest = remaining:sub(pos + #sep)

        local new_prefix = {}
        for _, p in ipairs(prefix) do new_prefix[#new_prefix + 1] = p end
        new_prefix[#new_prefix + 1] = segment


        local inner = rest:match("^{(.*)}$")
        if inner then
          local items = split_top_level(inner, ",")
          for i = #items, 1, -1 do
            local cp = {}
            for _, p in ipairs(new_prefix) do cp[#cp + 1] = p end
            stack[#stack + 1] = { cp, items[i] }
          end
        else
          stack[#stack + 1] = { new_prefix, rest }
        end
      end
    end
  end

  return results
end

local SECTION = {
  Import = 1, Module = 2, Constant = 3, Type = 4, Trait = 5,
  Impl = 6, Function = 7, Class = 8, Macro = 9,
}

local SECTION_HEADER = {
  [1] = "imports:", [2] = "mod:", [3] = "consts:", [4] = "types:",
  [5] = "traits:", [6] = "impls:", [7] = "fns:", [8] = "classes:",
  [9] = "macros:",
}

local CHILD_DETAILED = "detailed"
local CHILD_BRIEF = "brief"

local function new_entry(section, node, text)
  return {
    section = section,
    line_start = line_start(node),
    line_end = line_end(node),
    kind = "item",
    text = text,
    children = {},
    attrs = {},
    child_kind = CHILD_DETAILED,
  }
end

local function new_import_entry(node, paths, keyword)
  return {
    section = SECTION.Import,
    line_start = line_start(node),
    line_end = line_end(node),
    kind = "import",
    paths = paths,
    keyword = keyword,
  }
end

-- Walks backwards through siblings to find where doc comments and attrs begin,
-- so the line range covers the full item including its annotations.
local function doc_comment_start_line(node, source, is_doc_comment_fn, is_attr_fn)
  local earliest = nil
  local prev = node:prev_sibling()
  while prev do
    if is_attr_fn and is_attr_fn(prev) then
      prev = prev:prev_sibling()
    elseif is_doc_comment_fn(prev, source) then
      earliest = line_start(prev)
      prev = prev:prev_sibling()
    else
      break
    end
  end
  return earliest
end

local function collect_preceding_attrs(node, is_attr_fn)
  if not is_attr_fn then return {} end
  local attrs = {}
  local prev = node:prev_sibling()
  while prev do
    if is_attr_fn(prev) then
      attrs[#attrs + 1] = prev
      prev = prev:prev_sibling()
    else
      break
    end
  end
  local n = #attrs
  for i = 1, math.floor(n / 2) do
    attrs[i], attrs[n - i + 1] = attrs[n - i + 1], attrs[i]
  end
  return attrs
end

-- Module-level docs (//! in Rust, top-level docstrings in Python) sit before any real item.
-- We collect them as a range and stop at the first non-doc, non-attr node.
local function detect_module_doc(root, source, is_module_doc_fn, is_attr_fn)
  if not is_module_doc_fn then return nil end
  local start_line, end_line
  for _, child in ipairs(root:children()) do
    if is_module_doc_fn(child, source) then
      local l = line_start(child)
      if not start_line then start_line = l end
      local er, ec = child:end_()
      local el
      if ec == 0 then el = er else el = er + 1 end
      end_line = el
    elseif not (is_attr_fn and is_attr_fn(child)) and not child:extra() then
      break
    end
  end
  if start_line then return { start_line, end_line } end
  return nil
end

local function format_skeleton(entries, test_lines, module_doc, import_sep)
  local out = {}

  if module_doc then
    out[#out + 1] = "module doc: " .. format_range(module_doc[1], module_doc[2])
  end

  local grouped = {}
  for _, entry in ipairs(entries) do
    local s = entry.section
    if not grouped[s] then grouped[s] = {} end
    local g = grouped[s]
    g[#g + 1] = entry
  end

  local section_order = { SECTION.Import, SECTION.Module, SECTION.Constant, SECTION.Type,
    SECTION.Trait, SECTION.Impl, SECTION.Function, SECTION.Class, SECTION.Macro }

  for _, sec in ipairs(section_order) do
    local items = grouped[sec]
    if items and #items > 0 then
      local header = SECTION_HEADER[sec]
      if sec == SECTION.Import then
        local min_line, max_line = MAX_INT, 0
        for _, e in ipairs(items) do
          if e.line_start < min_line then min_line = e.line_start end
          if e.line_end > max_line then max_line = e.line_end end
        end

        if #out > 0 then out[#out + 1] = "" end
        out[#out + 1] = "imports: " .. format_range(min_line, max_line)

        -- Imports get merged into a trie so `use std::io` and `use std::fs`
        -- collapse into a single `std::{fs, io}` line.
        local keyword_order = {}
        local keyword_tries = {}
        for _, entry in ipairs(items) do
          local kw = entry.keyword or "import"
          if not keyword_tries[kw] then
            keyword_tries[kw] = new_trie()
            keyword_order[#keyword_order + 1] = kw
          end
          local trie = keyword_tries[kw]
          for _, path in ipairs(entry.paths) do
            trie_insert(trie, path)
          end
        end

        table.sort(keyword_order)

        for _, kw in ipairs(keyword_order) do
          local trie = keyword_tries[kw]
          local lines = render_children(trie, import_sep)
          if kw == "import" then
            for _, line in ipairs(lines) do
              out[#out + 1] = "  " .. line
            end
          else
            for _, line in ipairs(lines) do
              out[#out + 1] = "  " .. kw .. ": " .. line
            end
          end
        end

      elseif sec == SECTION.Module then
        local min_line, max_line = MAX_INT, 0
        for _, e in ipairs(items) do
          if e.line_start < min_line then min_line = e.line_start end
          if e.line_end > max_line then max_line = e.line_end end
        end
        if #out > 0 then out[#out + 1] = "" end
        out[#out + 1] = header .. " " .. format_range(min_line, max_line)
        local names = {}
        for _, e in ipairs(items) do names[#names + 1] = e.text end
        for _, line in ipairs(wrap_csv(names, "  ")) do
          out[#out + 1] = line
        end

      else
        if #out > 0 then out[#out + 1] = "" end
        out[#out + 1] = header
        for _, entry in ipairs(items) do
          if entry.kind == "item" then
            for _, attr in ipairs(entry.attrs) do
              out[#out + 1] = "  " .. attr
            end
            out[#out + 1] = "  " .. entry.text .. " " .. format_range(entry.line_start, entry.line_end)
            if entry.child_kind == CHILD_BRIEF and #entry.children > 0 then
              for _, line in ipairs(wrap_csv(entry.children, "    ")) do
                out[#out + 1] = line
              end
            else
              for _, child in ipairs(entry.children) do
                out[#out + 1] = "    " .. child
              end
            end
          end
        end
      end
    end
  end

  if test_lines and #test_lines > 0 then
    local min_t, max_t = MAX_INT, 0
    for _, l in ipairs(test_lines) do
      if l < min_t then min_t = l end
      if l > max_t then max_t = l end
    end
    if #out > 0 then out[#out + 1] = "" end
    out[#out + 1] = "tests: " .. format_range(min_t, max_t)
  end

  if #out == 0 then return "" end
  return table.concat(out, "\n") .. "\n"
end

-- Rust extractor

local function rust_is_attr(node)
  return node:type() == "attribute_item"
end

local function rust_is_doc_comment(node, source)
  if node:type() ~= "line_comment" then return false end
  local text = get_text(node, source)
  return text:sub(1, 3) == "///" and text:sub(1, 4) ~= "////"
end

local function rust_is_module_doc(node, source)
  if node:type() ~= "line_comment" then return false end
  local text = get_text(node, source)
  return text:sub(1, 3) == "//!"
end

local function rust_is_test_node(node, source, attrs)
  local kind = node:type()
  if kind ~= "mod_item" and kind ~= "function_item" then return false end
  for _, a in ipairs(attrs) do
    local text = get_text(a, source)
    if text == "#[test]" or text == "#[cfg(test)]" or text:sub(-7) == "::test]" then
      return true
    end
  end
  return false
end

local function rust_vis_prefix(node, source)
  for _, child in ipairs(node:children()) do
    if child:type() == "visibility_modifier" then
      return get_text(child, source)
    end
  end
  return ""
end

local function rust_prefixed(vis, rest)
  if vis == "" then return rest end
  return vis .. " " .. rest
end

local function rust_relevant_attr_texts(attrs, source)
  local result = {}
  for _, a in ipairs(attrs) do
    local text = get_text(a, source)
    if text:find("derive") or text:find("cfg") then
      result[#result + 1] = text
    end
  end
  return result
end

local function rust_fn_signature(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local params_node = find_child(node, "parameters")
  local params = params_node and get_text(params_node, source) or "()"
  local ret_node = node:field("return_type")[1]
  local ret = ""
  if ret_node then
    local t = get_text(ret_node, source)
    if t:sub(1, 2) == "->" then
      ret = " " .. t
    else
      ret = " -> " .. t
    end
  end
  return compact_ws(name .. params .. ret)
end

local function rust_extract_use(node, source, import_sep)
  local tree = find_child(node, "use_declaration") or node
  local argument = find_child(tree, "scoped_identifier")
    or find_child(tree, "use_wildcard")
    or find_child(tree, "use_list")
    or find_child(tree, "scoped_use_list")
    or find_child(tree, "identifier")

  local text
  if argument then
    text = get_text(argument, source)
  else
    local full = get_text(node, source)
    text = full:match("^use (.-)%;?$") or full:gsub(";$", "")
  end

  local paths = expand_import(text, import_sep)
  return new_import_entry(node, paths)
end

local function rust_extract_fields(node, source)
  local body = find_child(node, "field_declaration_list")
    or find_child(node, "enum_variant_list")
  if not body then return {} end

  local fields = {}
  local total = 0

  for _, child in ipairs(body:children()) do
    local ckind = child:type()
    if ckind == "field_declaration" then
      total = total + 1
      local vis = rust_vis_prefix(child, source)
      local fname = child:field("name")[1]
      local fname_text = fname and get_text(fname, source) or "_"
      local ftype = child:field("type")[1]
      local ftype_text = ftype and get_text(ftype, source) or "_"
      if total <= FIELD_TRUNCATE_THRESHOLD or vis ~= "" then
        fields[#fields + 1] = rust_prefixed(vis, fname_text .. ": " .. ftype_text)
      end
    elseif ckind == "enum_variant" then
      total = total + 1
      if total <= FIELD_TRUNCATE_THRESHOLD then
        local vname = child:field("name")[1]
        fields[#fields + 1] = vname and get_text(vname, source) or "_"
      end
    end
  end

  if total > FIELD_TRUNCATE_THRESHOLD and #fields < total then
    fields[#fields + 1] = truncated_msg(total)
  end

  return fields
end

local function rust_extract_struct_or_enum(node, source, attrs)
  local vis = rust_vis_prefix(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local generics_node = find_child(node, "type_parameters")
  local generics = generics_node and get_text(generics_node, source) or ""

  local kind_str = node:type():gsub("_item", ""):gsub("_definition", "")
  local text = rust_prefixed(vis, kind_str .. " " .. name .. generics)
  local is_enum = find_child(node, "enum_variant_list") ~= nil
  local children = rust_extract_fields(node, source)
  local attr_texts = rust_relevant_attr_texts(attrs, source)

  local entry = new_entry(SECTION.Type, node, text)
  entry.children = children
  entry.attrs = attr_texts
  if is_enum then entry.child_kind = CHILD_BRIEF end
  return entry
end

local function rust_extract_fn(node, source)
  local vis = rust_vis_prefix(node, source)
  local sig = rust_fn_signature(node, source)
  if not sig then return nil end
  return new_entry(SECTION.Function, node, rust_prefixed(vis, sig))
end

local function rust_extract_methods(node, source, include_vis)
  local body = find_child(node, "declaration_list")
  if not body then return {} end

  local methods = {}
  for _, child in ipairs(body:children()) do
    local ckind = child:type()
    if ckind == "function_item" or ckind == "function_signature_item" then
      local sig = rust_fn_signature(child, source)
      if sig then
        local lr = format_range(line_start(child), line_end(child))
        if include_vis then
          local vis = rust_vis_prefix(child, source)
          methods[#methods + 1] = rust_prefixed(vis, sig) .. " " .. lr
        else
          methods[#methods + 1] = sig .. " " .. lr
        end
      end
    end
  end
  return methods
end

local function rust_extract_trait(node, source)
  local vis = rust_vis_prefix(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local generics_node = find_child(node, "type_parameters")
  local generics = generics_node and get_text(generics_node, source) or ""

  local text = rust_prefixed(vis, name .. generics)
  local children = rust_extract_methods(node, source, false)
  local entry = new_entry(SECTION.Trait, node, text)
  entry.children = children
  return entry
end

local function rust_extract_impl(node, source)
  local type_node = node:field("type")[1]
    or find_child(node, "type_identifier")
  if not type_node then return nil end
  local type_name = get_text(type_node, source)

  local trait_node = node:field("trait")[1]
  local text
  if trait_node then
    text = get_text(trait_node, source) .. " for " .. type_name
  else
    text = type_name
  end

  local children = rust_extract_methods(node, source, true)
  local entry = new_entry(SECTION.Impl, node, text)
  entry.children = children
  return entry
end

local function rust_extract_const_or_static(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local type_node = node:field("type")[1]
  local type_str = type_node and (": " .. get_text(type_node, source)) or ""
  local vis = rust_vis_prefix(node, source)
  local prefix = node:type() == "static_item" and "static " or ""
  return new_entry(SECTION.Constant, node, rust_prefixed(vis, prefix .. name .. type_str))
end

local function rust_extract_mod(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local vis = rust_vis_prefix(node, source)
  return new_entry(SECTION.Module, node, rust_prefixed(vis, name))
end

local function rust_extract_macro(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  return new_entry(SECTION.Macro, node, name .. "!")
end

local function rust_extract_type_alias(node, source)
  local vis = rust_vis_prefix(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local val_node = node:field("type")[1]
  local val = val_node and (" = " .. get_text(val_node, source)) or ""
  return new_entry(SECTION.Type, node, rust_prefixed(vis, "type " .. name .. val))
end

local function rust_extract_node(node, source, attrs)
  local kind = node:type()
  if kind == "use_declaration" then
    return { rust_extract_use(node, source, "::") }
  elseif kind == "struct_item" or kind == "enum_item" or kind == "union_item" then
    local e = rust_extract_struct_or_enum(node, source, attrs)
    return e and { e } or {}
  elseif kind == "function_item" then
    local e = rust_extract_fn(node, source)
    return e and { e } or {}
  elseif kind == "trait_item" then
    local e = rust_extract_trait(node, source)
    return e and { e } or {}
  elseif kind == "impl_item" then
    local e = rust_extract_impl(node, source)
    return e and { e } or {}
  elseif kind == "const_item" or kind == "static_item" then
    local e = rust_extract_const_or_static(node, source)
    return e and { e } or {}
  elseif kind == "mod_item" then
    local e = rust_extract_mod(node, source)
    return e and { e } or {}
  elseif kind == "macro_definition" then
    local e = rust_extract_macro(node, source)
    return e and { e } or {}
  elseif kind == "type_item" then
    local e = rust_extract_type_alias(node, source)
    return e and { e } or {}
  end
  return {}
end

local function rust_extract(source, root)
  local entries = {}
  local test_lines = {}

  for _, child in ipairs(root:children()) do
    if rust_is_attr(child) or rust_is_doc_comment(child, source) then
    else
      local attrs = collect_preceding_attrs(child, rust_is_attr)
      if rust_is_test_node(child, source, attrs) then
        test_lines[#test_lines + 1] = line_start(child)
      else
        local extracted = rust_extract_node(child, source, attrs)
        for i, entry in ipairs(extracted) do
          if i == 1 then
            local doc_start = doc_comment_start_line(child, source, rust_is_doc_comment, rust_is_attr)
            if doc_start and doc_start < entry.line_start then
              entry.line_start = doc_start
            end
          end
          entries[#entries + 1] = entry
        end
      end
    end
  end

  local module_doc = detect_module_doc(root, source, rust_is_module_doc, rust_is_attr)
  return format_skeleton(entries, test_lines, module_doc, "::")
end

-- Python extractor

local function python_is_module_doc(node, source)
  if node:type() ~= "expression_statement" then return false end
  local first = node:child(0)
  if not first then return false end
  return first:type() == "string" and get_text(first, source):sub(1, 3) == '"""'
end

local function python_extract_import(node, source)
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

local function python_extract_class(node, source)
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

local function python_extract_function(node, source)
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

local function python_extract_assignment(node, source)
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

local function python_extract_node(node, source)
  local kind = node:type()
  if kind == "import_statement" or kind == "import_from_statement" then
    return { python_extract_import(node, source) }
  elseif kind == "class_definition" then
    local e = python_extract_class(node, source)
    return e and { e } or {}
  elseif kind == "function_definition" then
    local e = python_extract_function(node, source)
    return e and { e } or {}
  elseif kind == "decorated_definition" then
    local inner = find_child(node, "class_definition")
      or find_child(node, "function_definition")
    if inner and inner:type() == "class_definition" then
      local e = python_extract_class(inner, source)
      if e then
        e.line_start = line_start(node)
      end
      return e and { e } or {}
    elseif inner then
      local e = python_extract_function(node, source)
      return e and { e } or {}
    end
    return {}
  elseif kind == "expression_statement" then
    local first = node:child(0)
    if first and first:type() == "assignment" then
      local e = python_extract_assignment(first, source)
      return e and { e } or {}
    end
    return {}
  end
  return {}
end

local function python_extract(source, root)
  local entries = {}

  for _, child in ipairs(root:children()) do
    local extracted = python_extract_node(child, source)
    for _, entry in ipairs(extracted) do
      entries[#entries + 1] = entry
    end
  end

  local module_doc = detect_module_doc(root, source, python_is_module_doc, nil)
  return format_skeleton(entries, {}, module_doc, ".")
end

-- TypeScript / JavaScript extractor

local function ts_return_type(node, source)
  local r = get_text(node, source)
  if r:sub(1, 1) == ":" then return r end
  return ": " .. r
end

local function ts_class_member_sig(node, source)
  local mn_node = node:field("name")[1]
  local mn = mn_node and get_text(mn_node, source) or "_"
  local params_node = node:field("parameters")[1]
  local params = params_node and get_text(params_node, source) or ""
  local ret_node = node:field("return_type")[1]
  local ret = ret_node and ts_return_type(ret_node, source) or ""
  return mn .. params .. ret
end

local function ts_is_doc_comment(node, source)
  return node:type() == "comment" and get_text(node, source):sub(1, 3) == "/**"
end

local function ts_is_exported(node)
  local parent = node:parent()
  return parent and parent:type() == "export_statement"
end

local function ts_export_prefix(node)
  if ts_is_exported(node) then return "export " end
  return ""
end

local function ts_extract_import(node, source)
  local text = get_text(node, source)
  local cleaned = text:match("^import (.+)") or text
  cleaned = cleaned:gsub(";%s*$", "")
  return new_import_entry(node, { { cleaned } })
end

local function ts_extract_class(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local body_node = node:field("body")[1]
  if not body_node then return nil end

  local methods = {}
  local field_counts = {}
  for _, child in ipairs(body_node:children()) do
    local ckind = child:type()
    if ckind == "method_definition" then
      local sig = ts_class_member_sig(child, source)
      if sig then
        local lr = format_range(line_start(child), line_end(child))
        methods[#methods + 1] = sig .. " " .. lr
      end
    elseif ckind == "public_field_definition" or ckind == "property_definition" then
      local counter = "field"
      field_counts[counter] = (field_counts[counter] or 0) + 1
      if field_counts[counter] <= FIELD_TRUNCATE_THRESHOLD then
        local sig = ts_class_member_sig(child, source)
        if sig then
          local lr = format_range(line_start(child), line_end(child))
          methods[#methods + 1] = sig .. " " .. lr
        end
      end
    end
  end
  for _, count in pairs(field_counts) do
    if count > FIELD_TRUNCATE_THRESHOLD then
      methods[#methods + 1] = truncated_msg(count)
    end
  end

  local ep = ts_export_prefix(node)
  local entry = new_entry(SECTION.Class, node, ep .. name)
  entry.children = methods
  return entry
end

local function ts_extract_function(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local params_node = node:field("parameters")[1]
  local params = params_node and get_text(params_node, source) or "()"
  local ret_node = node:field("return_type")[1]
  local ret_str = ret_node and ts_return_type(ret_node, source) or ""

  local ep = ts_export_prefix(node)
  return new_entry(SECTION.Function, node, compact_ws(ep .. name .. params .. ret_str))
end

local function ts_extract_interface(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local body_node = node:field("body")[1]
  if not body_node then return nil end

  local fields = {}
  for _, child in ipairs(body_node:children()) do
    local ckind = child:type()
    if ckind == "property_signature" or ckind == "method_signature" then
      local text = get_text(child, source)
      text = text:gsub("[,;]%s*$", "")
      fields[#fields + 1] = text
    end
  end

  local ep = ts_export_prefix(node)
  local entry = new_entry(SECTION.Type, node, ep .. "interface " .. name)
  entry.children = fields
  return entry
end

local function ts_extract_type_alias(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local val_node = node:field("value")[1]
  local val_str = val_node and (" = " .. truncate(get_text(val_node, source), 80)) or ""
  local ep = ts_export_prefix(node)
  return new_entry(SECTION.Type, node, ep .. "type " .. name .. val_str)
end

local function ts_extract_enum(node, source)
  local name_node = node:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local ep = ts_export_prefix(node)
  return new_entry(SECTION.Type, node, ep .. "enum " .. name)
end

local function ts_extract_const(node, source)
  local decl = find_child(node, "variable_declarator")
  if not decl then return nil end
  local name_node = decl:field("name")[1]
  if not name_node then return nil end
  local name = get_text(name_node, source)
  local type_node = decl:field("type")[1]
  local type_str = type_node and ts_return_type(type_node, source) or ""
  local val_node = decl:field("value")[1]
  local val_str = val_node and (" = " .. truncate(get_text(val_node, source), 60)) or ""
  local ep = ts_export_prefix(node)
  return new_entry(SECTION.Constant, node, ep .. name .. type_str .. val_str)
end

local function ts_extract_lexical_declaration(node, source)
  local first = node:child(0)
  if not first then return nil end
  local kind_text = get_text(first, source)
  if kind_text == "const" then
    return ts_extract_const(node, source)
  end
  return nil
end

local function ts_extract_export_statement(node, source)
  for _, child in ipairs(node:children()) do
    local ckind = child:type()
    if ckind == "class_declaration" then return ts_extract_class(child, source)
    elseif ckind == "function_declaration" then return ts_extract_function(child, source)
    elseif ckind == "interface_declaration" then return ts_extract_interface(child, source)
    elseif ckind == "type_alias_declaration" then return ts_extract_type_alias(child, source)
    elseif ckind == "lexical_declaration" then return ts_extract_lexical_declaration(child, source)
    elseif ckind == "enum_declaration" then return ts_extract_enum(child, source)
    end
  end
  return nil
end

local function ts_extract_node(node, source)
  local kind = node:type()
  if kind == "import_statement" then
    return { ts_extract_import(node, source) }
  elseif kind == "class_declaration" then
    local e = ts_extract_class(node, source)
    return e and { e } or {}
  elseif kind == "function_declaration" then
    local e = ts_extract_function(node, source)
    return e and { e } or {}
  elseif kind == "interface_declaration" then
    local e = ts_extract_interface(node, source)
    return e and { e } or {}
  elseif kind == "type_alias_declaration" then
    local e = ts_extract_type_alias(node, source)
    return e and { e } or {}
  elseif kind == "enum_declaration" then
    local e = ts_extract_enum(node, source)
    return e and { e } or {}
  elseif kind == "lexical_declaration" then
    local e = ts_extract_lexical_declaration(node, source)
    return e and { e } or {}
  elseif kind == "export_statement" then
    local e = ts_extract_export_statement(node, source)
    return e and { e } or {}
  end
  return {}
end

local function ts_extract(source, root)
  local entries = {}

  for _, child in ipairs(root:children()) do
    if not ts_is_doc_comment(child, source) then
      local extracted = ts_extract_node(child, source)
      for i, entry in ipairs(extracted) do
        if i == 1 then
          local doc_start = doc_comment_start_line(child, source, ts_is_doc_comment, nil)
          if doc_start and doc_start < entry.line_start then
            entry.line_start = doc_start
          end
        end
        entries[#entries + 1] = entry
      end
    end
  end

  return format_skeleton(entries, {}, nil, "::")
end

-- Handler

local EXTRACTORS = {
  rust = rust_extract,
  python = python_extract,
  typescript = ts_extract,
  javascript = ts_extract,
}

maki.api.register_tool({
  name = "index",
  description = "Return a compact overview of a source file: imports, type definitions, function signatures, and structure with their line numbers surrounded by []. ~70-90% more efficient than reading the full file.\n\n- Use this FIRST to understand file structure before using read with offset/limit.\n- Supports source files in different programming languages and markdown.\n- Falls back with an error on unsupported languages. Use read instead.",
  schema = {
    type = "object",
    properties = {
      path = { type = "string", description = "Absolute path to the file" },
    },
    required = { "path" },
  },
  handler = function(input, ctx)
    local path = input.path
    if not path then
      return "error: path is required"
    end

    local ext = path:match("%.([^%.]+)$")
    if not ext then
      return "DELEGATE_NATIVE"
    end

    local lang = EXT_TO_LANG[ext]
    if not lang then
      return "DELEGATE_NATIVE"
    end

    local extractor = EXTRACTORS[lang]
    if not extractor then
      return "DELEGATE_NATIVE"
    end

    local ok, source = pcall(maki.fs.read, path)
    if not ok then
      return "error: " .. tostring(source)
    end

    local parser = maki.treesitter.get_parser(source, lang)
    local trees = parser:parse()
    local tree = trees[1]
    if not tree then
      return "error: tree-sitter failed to parse file"
    end

    local root = tree:root()
    local success, result = pcall(extractor, source, root)
    if not success then
      return "error: " .. tostring(result)
    end

    return result
  end,
})
