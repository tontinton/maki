local FIELD_TRUNCATE_THRESHOLD = 8
local LINE_WRAP_THRESHOLD = 120
local MAX_INT = math.maxinteger or (2 ^ 53)

local EXT_TO_LANG = {
  rs = "rust",
  py = "python",
  pyi = "python",
  ts = "typescript",
  tsx = "typescript",
  js = "javascript",
  jsx = "javascript",
  mjs = "javascript",
  cjs = "javascript",
  gleam = "gleam",
  go = "go",
  htm = "html",
  html = "html",
  java = "java",
  c = "c",
  h = "c",
  cpp = "cpp",
  cc = "cpp",
  cxx = "cpp",
  hpp = "cpp",
  hxx = "cpp",
  hh = "cpp",
  cs = "c_sharp",
  rb = "ruby",
  rake = "ruby",
  gemspec = "ruby",
  php = "php",
  swift = "swift",
  kt = "kotlin",
  kts = "kotlin",
  scala = "scala",
  sc = "scala",
  sh = "bash",
  bash = "bash",
  zsh = "bash",
  lua = "lua_lang",
  ex = "elixir",
  exs = "elixir",
  md = "markdown",
  markdown = "markdown",
  bzl = "bazel_bzl",
  zig = "zig",
  nix = "nix",
}

local FILENAME_TO_LANG = {
  ["MODULE.bazel"] = "bazel_module",
  ["BUILD"] = "bazel_build",
  ["BUILD.bazel"] = "bazel_build",
}

local LANG_TO_PARSER = {
  lua_lang = "lua",
  javascript = "typescript",
  bazel_build = "starlark",
  bazel_module = "starlark",
  bazel_bzl = "starlark",
}

local function parser_name(lang)
  return LANG_TO_PARSER[lang] or lang
end

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
  if s == e then
    return "[" .. s .. "]"
  end
  return "[" .. s .. "-" .. e .. "]"
end

local function ranged(text, range)
  return { body = text, range = range }
end

local function find_child(node, kind)
  for _, child in ipairs(node:children()) do
    if child:type() == kind then
      return child
    end
  end
  return nil
end

local function compact_ws(s)
  return (s:gsub("%s+", " "))
end

local function truncate(s, max)
  if #s <= max then
    return s
  end
  local boundary = max - 11
  if boundary < 0 then
    boundary = 0
  end
  local cut = s:sub(1, boundary)
  if cut:find("\n", 1, true) then
    return cut .. "\n[truncated]"
  end
  return cut .. "[truncated]"
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

-- Imports get broken into path segments and merged into a trie,
-- so `use std::io` and `use std::fs` collapse into `std::{fs, io}`.
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

-- Brace-aware separator search: nested groups like std::{fs, net::{TcpStream}}
-- need to split only at the top level, not inside braces.
local function find_sep(text, sep)
  local depth = 0
  local sep_len = #sep
  for i = 1, #text do
    local c = text:sub(i, i)
    if c == "{" then
      depth = depth + 1
    elseif c == "}" then
      if depth > 0 then
        depth = depth - 1
      end
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
    if c == 123 then
      depth = depth + 1
    elseif c == 125 then
      if depth > 0 then
        depth = depth - 1
      end
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

-- Turns `std::{fs, net::{TcpStream}}` into flat paths like
-- {"std", "fs"} and {"std", "net", "TcpStream"} by walking
-- brace groups with a stack instead of recursion.
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
        for _, p in ipairs(prefix) do
          path[#path + 1] = p
        end
        path[#path + 1] = remaining
        results[#results + 1] = path
      else
        local segment = remaining:sub(1, pos - 1)
        local rest = remaining:sub(pos + #sep)

        local new_prefix = {}
        for _, p in ipairs(prefix) do
          new_prefix[#new_prefix + 1] = p
        end
        new_prefix[#new_prefix + 1] = segment

        local inner = rest:match("^{(.*)}$")
        if inner then
          local items = split_top_level(inner, ",")
          for i = #items, 1, -1 do
            local cp = {}
            for _, p in ipairs(new_prefix) do
              cp[#cp + 1] = p
            end
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

local SECTIONS = {
  { key = "Import", header = "imports:" },
  { key = "Module", header = "mod:" },
  { key = "Constant", header = "consts:" },
  { key = "Type", header = "types:" },
  { key = "Trait", header = "traits:" },
  { key = "Impl", header = "impls:" },
  { key = "Function", header = "fns:" },
  { key = "Class", header = "classes:" },
  { key = "Macro", header = "macros:" },
  { key = "Heading", header = "headings:" },
}

local SECTION = {}
local SECTION_HEADER = {}
for i, def in ipairs(SECTIONS) do
  SECTION[def.key] = i
  SECTION_HEADER[i] = def.header
end

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

local function simple_import(node, source, prefixes, sep)
  local text = get_text(node, source)
  local cleaned = text
  for _, prefix in ipairs(prefixes) do
    local m = cleaned:match("^" .. prefix .. "(.*)")
    if m then
      cleaned = m
      break
    end
  end
  cleaned = cleaned:gsub(";%s*$", ""):match("^%s*(.-)%s*$")
  local paths = {}
  local path = {}
  for part in cleaned:gmatch("[^" .. sep:gsub("([%.%\\])", "%%%1") .. "]+") do
    path[#path + 1] = part
  end
  paths[1] = path
  return new_import_entry(node, paths)
end

local function prefixed(vis, rest)
  if vis == "" then
    return rest
  end
  return vis .. " " .. rest
end

local function extract_enum_variants(body, source, variant_kind)
  local values = {}
  for _, child in ipairs(body:children()) do
    if child:type() == variant_kind then
      local name_node = child:field("name")[1]
      local name = name_node and get_text(name_node, source) or "_"
      values[#values + 1] = name
    end
  end
  return values
end

local function extract_fields_truncated(body, source, field_kind, format_fn)
  local fields = {}
  local total = 0
  for _, child in ipairs(body:children()) do
    if child:type() == field_kind then
      total = total + 1
      if total <= FIELD_TRUNCATE_THRESHOLD then
        fields[#fields + 1] = format_fn(child, source)
      end
    end
  end
  if total > FIELD_TRUNCATE_THRESHOLD then
    fields[#fields + 1] = truncated_msg(total)
  end
  return fields
end

local function extract_body_members(body, source, rules)
  local members = {}
  local field_counts = {}
  for _, child in ipairs(body:children()) do
    local kind = child:type()
    local rule = nil
    for _, r in ipairs(rules) do
      if r.kind == kind then
        rule = r
        break
      end
    end
    if rule then
      if rule.handler == "method" then
        local sig = rule.fn(child, source)
        if sig then
          local lr = format_range(line_start(child), line_end(child))
          members[#members + 1] = ranged(sig, lr)
        end
      elseif rule.handler == "field" then
        local counter = rule.counter or kind
        field_counts[counter] = (field_counts[counter] or 0) + 1
        if field_counts[counter] <= FIELD_TRUNCATE_THRESHOLD then
          local text = rule.fn(child, source)
          local lr = format_range(line_start(child), line_end(child))
          members[#members + 1] = ranged(text, lr)
        end
      end
    end
  end
  for _, count in pairs(field_counts) do
    if count > FIELD_TRUNCATE_THRESHOLD then
      members[#members + 1] = truncated_msg(count)
    end
  end
  return members
end

-- Walks backwards from a node to find where its doc comments begin,
-- so the line range covers the full annotated item.
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
  if not is_attr_fn then
    return {}
  end
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

-- Module-level docs (like //! in Rust) sit before any real item.
-- We collect them as a range and stop at the first non-doc node.
local function detect_module_doc(root, source, is_module_doc_fn, is_attr_fn)
  if not is_module_doc_fn then
    return nil
  end
  local start_line, end_line_val
  for _, child in ipairs(root:children()) do
    if is_module_doc_fn(child, source) then
      local l = line_start(child)
      if not start_line then
        start_line = l
      end
      local er, ec = child:end_()
      local el
      if ec == 0 then
        el = er
      else
        el = er + 1
      end
      end_line_val = el
    elseif not (is_attr_fn and is_attr_fn(child)) and not child:extra() then
      break
    end
  end
  if start_line then
    return { start_line, end_line_val }
  end
  return nil
end

local render_item_lines

local TRUNCATED_SUFFIX = "[truncated]"
local TRUNCATED_INFIX = " more truncated]"

local function ends_with_truncation(s)
  if s:sub(-#TRUNCATED_SUFFIX) == TRUNCATED_SUFFIX then
    return true
  end
  return s:sub(1, 1) == "[" and s:sub(-#TRUNCATED_INFIX) == TRUNCATED_INFIX
end

local function emit_body_with_range(body, range, out, meta)
  if body:find("\n", 1, true) then
    local leading = body:match("^(%s*)") or ""
    local lines = {}
    for line in (body .. "\n"):gmatch("([^\n]*)\n") do
      lines[#lines + 1] = line
    end
    for i, line in ipairs(lines) do
      if i == 1 then
        out[#out + 1] = line .. " " .. range
        meta[#out] = { body = line, range = range }
      else
        line = leading .. line
        out[#out + 1] = line
        if ends_with_truncation(line) then
          meta[#out] = { tag = "dim" }
        end
      end
    end
  else
    out[#out + 1] = body .. " " .. range
    if ends_with_truncation(body) then
      meta[#out] = { tag = "dim" }
    else
      meta[#out] = { body = body, range = range }
    end
  end
end

render_item_lines = function(entry, out, indent, meta)
  local prefix = string.rep(" ", indent)
  for _, attr in ipairs(entry.attrs or {}) do
    out[#out + 1] = prefix .. attr
  end
  local range = format_range(entry.line_start, entry.line_end)
  local body = prefix .. entry.text
  emit_body_with_range(body, range, out, meta)
  if entry.child_kind == CHILD_BRIEF and #(entry.children or {}) > 0 then
    local children = entry.children
    local last = children[#children]
    local has_trunc = type(last) == "string" and ends_with_truncation(last)
    if has_trunc then
      children = { table.unpack(children, 1, #children - 1) }
    end
    local csv_items = {}
    for _, c in ipairs(children) do
      csv_items[#csv_items + 1] = type(c) == "table" and (c.body .. " " .. c.range) or c
    end
    for _, line in ipairs(wrap_csv(csv_items, prefix .. "  ")) do
      out[#out + 1] = line
      meta[#out] = { tag = "dim" }
    end
    if has_trunc then
      out[#out + 1] = prefix .. "  " .. last
      meta[#out] = { tag = "dim" }
    end
  else
    for _, child in ipairs(entry.children or {}) do
      if type(child) == "table" and child.body then
        local cbody = prefix .. "  " .. child.body
        emit_body_with_range(cbody, child.range, out, meta)
      elseif type(child) == "table" then
        render_item_lines(child, out, indent + 2, meta)
      else
        out[#out + 1] = prefix .. "  " .. child
        if type(child) == "string" and ends_with_truncation(child) then
          meta[#out] = { tag = "dim" }
        end
      end
    end
  end
end

local function format_skeleton(entries, test_lines, module_doc, import_sep)
  local out = {}
  local meta = {}

  local function emit_section(label, range)
    out[#out + 1] = label .. (range or "")
    meta[#out] = range and { tag = "section", body = label, range = range } or { tag = "section" }
  end

  local function items_range(items)
    local lo, hi = MAX_INT, 0
    for _, e in ipairs(items) do
      if e.line_start < lo then
        lo = e.line_start
      end
      if e.line_end > hi then
        hi = e.line_end
      end
    end
    return format_range(lo, hi)
  end

  local function lines_range(lines)
    local lo, hi = MAX_INT, 0
    for _, l in ipairs(lines) do
      if l < lo then
        lo = l
      end
      if l > hi then
        hi = l
      end
    end
    return format_range(lo, hi)
  end

  if module_doc then
    emit_section("module doc: ", format_range(module_doc[1], module_doc[2]))
  end

  local grouped = {}
  for _, entry in ipairs(entries) do
    local s = entry.section
    if not grouped[s] then
      grouped[s] = {}
    end
    local g = grouped[s]
    g[#g + 1] = entry
  end

  for _, sec_def in ipairs(SECTIONS) do
    local sec = SECTION[sec_def.key]
    local items = grouped[sec]
    if items and #items > 0 then
      local header = SECTION_HEADER[sec]
      if sec == SECTION.Import then
        if #out > 0 then
          out[#out + 1] = ""
        end
        emit_section("imports: ", items_range(items))

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
        if #out > 0 then
          out[#out + 1] = ""
        end
        emit_section(header .. " ", items_range(items))
        local names = {}
        for _, e in ipairs(items) do
          names[#names + 1] = e.text
        end
        for _, line in ipairs(wrap_csv(names, "  ")) do
          out[#out + 1] = line
        end
      elseif sec == SECTION.Heading then
        if #out > 0 then
          out[#out + 1] = ""
        end
        emit_section(header)
        for _, entry in ipairs(items) do
          local range = format_range(entry.line_start, entry.line_end)
          local body = "  " .. entry.text
          out[#out + 1] = body .. " " .. range
          meta[#out] = { body = body, range = range }
        end
      else
        if #out > 0 then
          out[#out + 1] = ""
        end
        emit_section(header)
        for _, entry in ipairs(items) do
          if entry.kind == "item" then
            render_item_lines(entry, out, 2, meta)
          end
        end
      end
    end
  end

  if test_lines and #test_lines > 0 then
    if #out > 0 then
      out[#out + 1] = ""
    end
    emit_section("tests: ", lines_range(test_lines))
  end

  if #out == 0 then
    return "", {}
  end
  return table.concat(out, "\n") .. "\n", meta
end

local U = {
  SECTION = SECTION,
  FIELD_TRUNCATE_THRESHOLD = FIELD_TRUNCATE_THRESHOLD,
  CHILD_DETAILED = CHILD_DETAILED,
  CHILD_BRIEF = CHILD_BRIEF,
  get_text = get_text,
  line_start = line_start,
  line_end = line_end,
  find_child = find_child,
  compact_ws = compact_ws,
  TRUNCATED_SUFFIX = TRUNCATED_SUFFIX,
  truncate = truncate,
  truncated_msg = truncated_msg,
  new_entry = new_entry,
  new_import_entry = new_import_entry,
  simple_import = simple_import,
  expand_import = expand_import,
  format_range = format_range,
  ranged = ranged,
  prefixed = prefixed,
  extract_enum_variants = extract_enum_variants,
  extract_fields_truncated = extract_fields_truncated,
  extract_body_members = extract_body_members,
  format_skeleton = format_skeleton,
}

local function default_extract(lang, source, root)
  local entries = {}
  local test_lines = {}

  local is_doc = lang.is_doc_comment
  local is_attr = lang.is_attr
  local is_test = lang.is_test_node

  for _, child in ipairs(root:children()) do
    if (is_attr and is_attr(child)) or (is_doc and is_doc(child, source)) then
      -- these get attached to the next real item instead
    else
      local attrs = collect_preceding_attrs(child, is_attr)
      if is_test and is_test(child, source, attrs) then
        test_lines[#test_lines + 1] = line_start(child)
      else
        local extracted = lang.extract_nodes(child, source, attrs)
        for i, entry in ipairs(extracted) do
          if i == 1 and is_doc then
            local doc_start = doc_comment_start_line(child, source, is_doc, is_attr)
            if doc_start and doc_start < entry.line_start then
              entry.line_start = doc_start
            end
          end
          entries[#entries + 1] = entry
        end
      end
    end
  end

  local module_doc = detect_module_doc(root, source, lang.is_module_doc, is_attr)
  return format_skeleton(entries, test_lines, module_doc, lang.import_separator)
end

local function validate_lang(name, lang)
  if lang.extract then
    assert(type(lang.extract) == "function", name .. ": extract must be a function")
    return
  end
  assert(type(lang.extract_nodes) == "function", name .. ": missing extract_nodes")
  assert(type(lang.import_separator) == "string", name .. ": missing import_separator")
  for _, opt in ipairs({ "is_doc_comment", "is_module_doc", "is_attr", "is_test_node" }) do
    local v = lang[opt]
    assert(v == nil or type(v) == "function", name .. ": " .. opt .. " must be nil or function")
  end
end

local LANG_ALIASES = {
  javascript = "typescript",
}

local function unique_langs()
  local seen = {}
  local langs = {}
  for _, lang in pairs(EXT_TO_LANG) do
    local resolved = LANG_ALIASES[lang] or lang
    if not seen[resolved] then
      seen[resolved] = true
      langs[#langs + 1] = resolved
    end
  end
  table.sort(langs)
  return langs
end

local EXTRACTORS = {}

-- Bazel file-kind extractors live in lang/bazel/ and are pre-registered
-- because BUILD.bazel and MODULE.bazel are detected by filename (not by
-- extension), and all three share the "starlark" tree-sitter parser.
for _, sub in ipairs({ "build", "module", "bzl" }) do
  local lang_name = "bazel_" .. sub
  local factory = require("lang.bazel." .. sub)
  local lang = factory(U)
  validate_lang(lang_name, lang)
  EXTRACTORS[lang_name] = lang.extract
end

for _, name in ipairs(unique_langs()) do
  if not EXTRACTORS[name] then
    local factory = require("lang." .. name)
    local lang = factory(U)
    validate_lang(name, lang)

    if lang.extract then
      EXTRACTORS[name] = lang.extract
    else
      EXTRACTORS[name] = function(source, root)
        return default_extract(lang, source, root)
      end
    end
  end
end

for alias, target in pairs(LANG_ALIASES) do
  EXTRACTORS[alias] = EXTRACTORS[target]
end

local function index_source(source, lang_name)
  local extractor = EXTRACTORS[lang_name]
  if not extractor then
    return nil, "unsupported language: " .. tostring(lang_name)
  end
  local pname = parser_name(lang_name)
  local parser = maki.treesitter.get_parser(source, pname)
  local root = parser:parse()[1]:root()
  return extractor(source, root)
end

local LANG_TO_EXT = {}
for ext, lang in pairs(EXT_TO_LANG) do
  if not LANG_TO_EXT[lang] then
    LANG_TO_EXT[lang] = ext
  end
end

return {
  index_source = index_source,
  EXT_TO_LANG = EXT_TO_LANG,
  LANG_TO_EXT = LANG_TO_EXT,
  FILENAME_TO_LANG = FILENAME_TO_LANG,
  TRUNCATED_SUFFIX = TRUNCATED_SUFFIX,
}
