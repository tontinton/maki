return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local format_skeleton = U.format_skeleton
  local line_start = U.line_start
  local line_end = U.line_end
  local format_range = U.format_range
  local ranged = U.ranged
  local SECTION = U.SECTION

  local MAX_LINE = 120

  local function is_sym(node)
    return node and node:type() == "sym_lit"
  end

  local function is_kwd(node)
    return node and node:type() == "kwd_lit"
  end

  local function node_text(node, source)
    return compact_ws(get_text(node, source))
  end

  local function line_text(node, source)
    return truncate(node_text(node, source), MAX_LINE)
  end

  local function dot_segments(text)
    local segments = {}
    for part in text:gmatch("[^.]+") do
      segments[#segments + 1] = part
    end
    return segments
  end

  local function sym_text(sym, source)
    local name = sym:field("name")[1]
    local text = name and get_text(name, source) or ""
    local ns = sym:field("namespace")[1]
    if ns then
      text = get_text(ns, source) .. "/" .. text
    end
    return text
  end

  local function sym_meta(sym, source)
    local parts = {}
    for _, field in ipairs({ "meta", "old_meta" }) do
      for _, m in ipairs(sym:field(field)) do
        parts[#parts + 1] = line_text(m, source)
      end
    end
    if #parts == 0 then
      return nil
    end
    return truncate(table.concat(parts, " "), MAX_LINE)
  end

  local function unwrap_meta(node)
    while node and (node:type() == "meta_lit" or node:type() == "old_meta_lit") do
      node = node:field("value")[1]
    end
    return node
  end

  local function each_branch(node, fn)
    local t = node:type()
    if t ~= "read_cond_lit" and t ~= "splicing_read_cond_lit" then
      fn(node)
      return
    end
    for _, v in ipairs(node:field("value")) do
      if not is_kwd(v) then
        if t == "splicing_read_cond_lit" and v:type() == "vec_lit" then
          for _, f in ipairs(v:field("value")) do
            fn(f)
          end
        else
          fn(v)
        end
      end
    end
  end

  local function quoted_value(node, source)
    if node and node:type() == "quoting_lit" then
      return unwrap_meta(node:field("value")[1])
    end
    if node and node:type() == "list_lit" then
      local values = node:field("value")
      if #values == 2 and is_sym(values[1]) and sym_text(values[1], source) == "quote" then
        return unwrap_meta(values[2])
      end
    end
    return nil
  end

  local function prefixed_lib(prefix, name)
    if prefix and name:find(".", 1, true) then
      return nil
    end
    return prefix and prefix .. "." .. name or name
  end

  local function symbol_vector(node, source)
    if not node or node:type() ~= "vec_lit" then
      return nil
    end
    local names = {}
    for _, value in ipairs(node:field("value")) do
      if not is_sym(value) then
        return nil
      end
      names[#names + 1] = sym_text(value, source)
    end
    return names
  end

  local function rename_map(node, source)
    if not node or node:type() ~= "map_lit" then
      return nil
    end
    local values = node:field("value")
    if #values % 2 ~= 0 then
      return nil
    end
    local renames = {}
    for i = 1, #values, 2 do
      if not is_sym(values[i]) or not is_sym(values[i + 1]) then
        return nil
      end
      local from = sym_text(values[i], source)
      if renames[from] then
        return nil
      end
      renames[from] = sym_text(values[i + 1], source)
    end
    return renames
  end

  local function libspec_options(values, source)
    local options = { excludes = {}, renames = {}, refers = {} }
    local seen = {}
    local i = 2
    while i <= #values do
      local option = values[i]
      local value = values[i + 1]
      if not is_kwd(option) or not value then
        return nil
      end
      local key = get_text(option, source)
      if seen[key] then
        return nil
      end
      seen[key] = true
      if key == ":as" or key == ":as-alias" then
        if options.alias or not is_sym(value) then
          return nil
        end
        options.alias = sym_text(value, source)
        options.as_alias = key == ":as-alias"
      elseif key == ":refer" then
        if is_kwd(value) and get_text(value, source) == ":all" then
          options.refer_all = true
        else
          local names = symbol_vector(value, source)
          if not names then
            return nil
          end
          for _, name in ipairs(names) do
            options.refers[#options.refers + 1] = name
          end
        end
      elseif key == ":refer-macros" or key == ":only" then
        local names = symbol_vector(value, source)
        if not names then
          return nil
        end
        for _, name in ipairs(names) do
          options.refers[#options.refers + 1] = name
        end
      elseif key == ":exclude" then
        local names = symbol_vector(value, source)
        if not names then
          return nil
        end
        for _, name in ipairs(names) do
          options.excludes[name] = true
        end
      elseif key == ":rename" then
        local renames = rename_map(value, source)
        if not renames then
          return nil
        end
        options.renames = renames
      end
      i = i + 2
    end
    return options
  end

  local function libspec(node, source, prefix)
    node = unwrap_meta(node)
    if is_sym(node) then
      local lib = prefixed_lib(prefix, sym_text(node, source))
      if not lib then
        return nil
      end
      return { lib = lib, node = node, text = lib }
    end
    if not node or node:type() ~= "vec_lit" then
      return nil
    end
    local values = node:field("value")
    if not values[1] then
      return nil
    end
    local lib = is_sym(values[1]) and prefixed_lib(prefix, sym_text(values[1], source))
    local parts = { lib or get_text(values[1], source) }
    for i = 2, #values do
      parts[#parts + 1] = get_text(values[i], source)
    end
    local options = lib and libspec_options(values, source) or {}
    return {
      alias = options.alias,
      as_alias = options.as_alias,
      excludes = options.excludes,
      lib = lib,
      node = node,
      refer_all = options.refer_all,
      refers = options.refers,
      renames = options.renames,
      text = truncate(compact_ws(table.concat(parts, " ")), MAX_LINE),
    }
  end

  local function libspecs(node, source, quoted, kind)
    node = quoted and quoted_value(node, source) or unwrap_meta(node)
    if not node then
      return {}
    end
    local specs = {}
    local function expand(value, prefix)
      local spec = libspec(value, source, prefix)
      if spec then
        spec.kind = spec.as_alias and "alias" or kind
        specs[#specs + 1] = spec
        return
      end
      value = unwrap_meta(value)
      if not value or value:type() ~= "list_lit" then
        return
      end
      local values = value:field("value")
      if not is_sym(values[1]) then
        return
      end
      local next_prefix = prefixed_lib(prefix, sym_text(values[1], source))
      if not next_prefix then
        return
      end
      for i = 2, #values do
        expand(values[i], next_prefix)
      end
    end
    local direct = libspec(node, source)
    if direct then
      direct.kind = direct.as_alias and "alias" or kind
      specs[1] = direct
    elseif node:type() == "list_lit" then
      local values = node:field("value")
      if is_sym(values[1]) then
        local prefix = sym_text(values[1], source)
        for i = 2, #values do
          expand(values[i], prefix)
        end
      end
    end
    return specs
  end

  local function resolve_libspec(spec, resolver)
    if spec.alias then
      resolver.aliases[spec.alias] = spec.lib
    end
    local excludes = spec.excludes or {}
    local renames = spec.renames or {}
    for _, name in ipairs(spec.refers or {}) do
      if not excludes[name] then
        resolver.refers[renames[name] or name] = { ns = spec.lib, name = name }
      end
    end
    if spec.refer_all or spec.kind == "use" or spec.kind == "use-macros" then
      for from, to in pairs(renames) do
        if not excludes[from] then
          resolver.refers[to] = { ns = spec.lib, name = from }
        end
      end
    end
  end

  local function string_value(node, source)
    node = unwrap_meta(node)
    if node and node:type() == "str_lit" then
      return get_text(node, source)
    end
    local quoted = quoted_value(node, source)
    if quoted and quoted:type() == "str_lit" then
      return get_text(quoted, source)
    end
    return nil
  end

  local function first_line_text(node, source)
    return line_text(node, source)
  end

  local signature_text

  local function unknown_def_text(values, source)
    local head = sym_text(values[1], source)
    local name = unwrap_meta(values[2])
    if not values[3] or (not is_sym(name) and not is_kwd(name)) then
      return nil
    end
    return truncate(head .. " " .. signature_text(get_text(name, source), values, source), MAX_LINE)
  end

  local function name_value(values, source)
    local raw = unwrap_meta(values[2])
    if not is_sym(raw) then
      return nil
    end
    return sym_meta(values[2], source), sym_text(raw, source)
  end

  signature_text = function(name, values, source)
    local params = {}
    local i = 3
    while values[i] do
      local v = values[i]
      local t = v:type()
      if t == "vec_lit" then
        params[#params + 1] = line_text(v, source)
        break
      elseif t == "list_lit" then
        local first = v:field("value")[1]
        if first and first:type() == "vec_lit" then
          params[#params + 1] = line_text(first, source)
          i = i + 1
        else
          break
        end
      elseif t == "kwd_lit" and get_text(v, source) == ":-" then
        i = i + 2
      elseif #params == 0 and (t == "str_lit" or t == "map_lit") then
        i = i + 1
      else
        break
      end
    end
    if #params == 0 then
      return name
    end
    return name .. " " .. table.concat(params, " ")
  end

  local function def_rule(node, values, source)
    local meta, name = name_value(values, source)
    if not name then
      return nil
    end
    local text = (meta and meta .. " " or "") .. name
    local val = values[3]
    if values[4] and val:type() == "str_lit" then
      val = values[4]
    end
    if val then
      text = text .. " = " .. first_line_text(val, source)
    end
    return new_entry(SECTION.Constant, node, truncate(text, MAX_LINE))
  end

  local function fn_rule(section)
    return function(node, values, source)
      local meta, name = name_value(values, source)
      if not name then
        return nil
      end
      local text = signature_text(name, values, source)
      if meta then
        text = meta .. " " .. text
      end
      return new_entry(section, node, truncate(text, MAX_LINE))
    end
  end

  local function defmulti_rule(node, values, source)
    local meta, name = name_value(values, source)
    if not name then
      return nil
    end
    return new_entry(SECTION.Function, node, truncate((meta and meta .. " " or "") .. name, MAX_LINE))
  end

  local function defmethod_rule(node, values, source)
    local meta, name = name_value(values, source)
    if not name then
      return nil
    end
    local text = (meta and meta .. " " or "") .. name
    local dispatch = values[3]
    if dispatch then
      text = text .. " " .. truncate(compact_ws(get_text(dispatch, source)), MAX_LINE)
    end
    local params = values[4]
    if params and params:type() == "vec_lit" then
      text = text .. " " .. compact_ws(get_text(params, source))
    end
    return new_entry(SECTION.Impl, node, truncate(text, MAX_LINE))
  end

  local function type_rule(node, values, source, has_fields)
    local meta, name = name_value(values, source)
    if not name then
      return nil
    end
    local entry = new_entry(SECTION.Type, node, truncate((meta and meta .. " " or "") .. name, MAX_LINE))
    local i = 3
    if has_fields then
      local fields = values[3]
      if fields and fields:type() == "vec_lit" then
        entry.text = truncate(entry.text .. " " .. line_text(fields, source), MAX_LINE)
        i = 4
      end
    end
    for j = i, #values do
      local m = values[j]
      if m:type() == "list_lit" then
        local mv = m:field("value")
        local mhead = mv[1]
        if is_sym(mhead) then
          local mtext = sym_text(mhead, source)
          local mparams = mv[2]
          if mparams and mparams:type() == "vec_lit" then
            mtext = truncate(mtext .. " " .. line_text(mparams, source), MAX_LINE)
          end
          entry.children[#entry.children + 1] = ranged(mtext, format_range(line_start(m), line_end(m)))
        end
      end
    end
    return entry
  end

  local function declare_rule(_, values, source)
    local entries = {}
    for i = 2, #values do
      local v = values[i]
      if is_sym(v) then
        entries[#entries + 1] = new_entry(SECTION.Constant, v, sym_text(v, source))
      end
    end
    return entries
  end

  local function spec_def_rule(node, values, source)
    local kw = unwrap_meta(values[2])
    if not kw or kw:type() ~= "kwd_lit" then
      return nil
    end
    return new_entry(SECTION.Rule, node, get_text(kw, source))
  end

  local function schema_def_rule(node, values, source)
    local meta, name = name_value(values, source)
    if not name then
      return nil
    end
    local text = (meta and meta .. " " or "") .. name
    for i = 3, #values - 1 do
      local v = values[i]
      if v:type() == "kwd_lit" and get_text(v, source) == ":-" then
        text = text .. " :- " .. first_line_text(values[i + 1], source)
        break
      end
    end
    return new_entry(SECTION.Constant, node, truncate(text, MAX_LINE))
  end

  local function ns_rule(node, values, source, resolver)
    resolver.aliases = {}
    resolver.refers = {}

    local entries = {}
    local name_node = unwrap_meta(values[2])
    if is_sym(name_node) then
      entries[#entries + 1] = new_entry(SECTION.Module, node, sym_text(name_node, source))
    end

    for i = 3, #values do
      each_branch(values[i], function(clause)
        if clause:type() ~= "list_lit" then
          return
        end
        local cv = clause:field("value")
        local head = cv[1]
        if not is_kwd(head) then
          return
        end
        local h = get_text(head, source)
        local kinds = {
          [":require"] = "require",
          [":require-macros"] = "require-macros",
          [":use"] = "use",
          [":use-macros"] = "use-macros",
        }
        local kind = kinds[h]
        if kind then
          for j = 2, #cv do
            each_branch(cv[j], function(value)
              for _, spec in ipairs(libspecs(value, source, false, kind)) do
                resolve_libspec(spec, resolver)
                entries[#entries + 1] = new_import_entry(spec.node, { { spec.text } }, spec.kind)
              end
            end)
          end
        elseif h == ":load" then
          for j = 2, #cv do
            local path = string_value(cv[j], source)
            if path then
              entries[#entries + 1] = new_import_entry(cv[j], { { path } }, "load")
            end
          end
        elseif h == ":import" then
          for j = 2, #cv do
            each_branch(cv[j], function(c)
              local t = c:type()
              if t == "list_lit" or t == "vec_lit" then
                local iv = c:field("value")
                if is_sym(iv[1]) then
                  local pkg = sym_text(iv[1], source)
                  local paths = {}
                  for m = 2, #iv do
                    if is_sym(iv[m]) then
                      local segments = dot_segments(pkg)
                      segments[#segments + 1] = sym_text(iv[m], source)
                      paths[#paths + 1] = segments
                    end
                  end
                  if #paths > 0 then
                    entries[#entries + 1] = new_import_entry(c, paths, "import")
                  end
                end
              elseif is_sym(c) then
                entries[#entries + 1] = new_import_entry(c, { dot_segments(sym_text(c, source)) }, "import")
              end
            end)
          end
        end
      end)
    end
    return entries
  end

  local function require_rule(_, values, source, resolver)
    local entries = {}
    for i = 2, #values do
      each_branch(values[i], function(value)
        for _, spec in ipairs(libspecs(value, source, true, "require")) do
          resolve_libspec(spec, resolver)
          entries[#entries + 1] = new_import_entry(spec.node, { { spec.text } }, spec.kind)
        end
      end)
    end
    return entries
  end

  local function load_rule(keyword)
    return function(_, values, source)
      local entries = {}
      for i = 2, #values do
        local path = string_value(values[i], source)
        if path then
          entries[#entries + 1] = new_import_entry(values[i], { { path } }, keyword)
        end
      end
      return entries
    end
  end

  local function_rule = fn_rule(SECTION.Function)

  local function type_rule_for(has_fields)
    return function(node, values, source)
      return type_rule(node, values, source, has_fields)
    end
  end

  local type_rule_with_fields = type_rule_for(true)
  local type_rule_without_fields = type_rule_for(false)

  local CORE_RULES = {
    ns = ns_rule,
    require = require_rule,
    load = load_rule("load"),
    ["load-file"] = load_rule("load-file"),
    def = def_rule,
    defonce = def_rule,
    defn = function_rule,
    ["defn-"] = function_rule,
    definline = function_rule,
    defmacro = fn_rule(SECTION.Macro),
    ["defmacro-"] = fn_rule(SECTION.Macro),
    defmulti = defmulti_rule,
    defmethod = defmethod_rule,
    defprotocol = type_rule_without_fields,
    definterface = type_rule_without_fields,
    defrecord = type_rule_with_fields,
    deftype = type_rule_with_fields,
    defstruct = type_rule_with_fields,
    declare = declare_rule,
  }

  local SPEC_RULES = { def = spec_def_rule }

  local SCHEMA_RULES = {
    def = schema_def_rule,
    defn = function_rule,
    ["defn-"] = function_rule,
    defrecord = type_rule_with_fields,
    deftype = type_rule_with_fields,
    defprotocol = type_rule_without_fields,
    defmethod = defmethod_rule,
  }

  local RULES_BY_NS = {
    ["clojure.core"] = CORE_RULES,
    ["cljs.core"] = CORE_RULES,
    ["clojure.spec.alpha"] = SPEC_RULES,
    ["cljs.spec.alpha"] = SPEC_RULES,
    ["schema.core"] = SCHEMA_RULES,
  }

  return {
    extract = function(source, root)
      local entries = {}
      local resolver = { aliases = {}, refers = {} }

      local function resolve_head(text)
        local ns, local_name = text:match("^([^/]+)/(.+)$")
        if not ns then
          local referred = resolver.refers[text]
          if referred then
            return referred.ns, referred.name
          end
          return "clojure.core", text
        end
        return resolver.aliases[ns] or ns, local_name
      end

      local function add(result)
        if not result then
          return
        end
        if result.section then
          entries[#entries + 1] = result
        else
          for _, entry in ipairs(result) do
            entries[#entries + 1] = entry
          end
        end
      end

      local function process(node)
        local t = node:type()
        if t == "dis_expr" or t == "quoting_lit" or t == "syn_quoting_lit" or t == "tagged_or_ctor_lit" then
          return
        end
        if t == "read_cond_lit" or t == "splicing_read_cond_lit" then
          each_branch(node, process)
          return
        end
        local values = node:field("value")
        if t ~= "list_lit" then
          for _, value in ipairs(values) do
            process(value)
          end
          return
        end
        local head = unwrap_meta(values[1])
        if not is_sym(head) then
          for _, value in ipairs(values) do
            process(value)
          end
          return
        end
        local head_text = sym_text(head, source)
        local ns, local_name = resolve_head(head_text)
        local rules = RULES_BY_NS[ns]
        local rule = rules and rules[local_name]
        if rule then
          add(rule(node, values, source, resolver))
        elseif local_name:match("^def[%w_-]*$") then
          local text = unknown_def_text(values, source)
          if text then
            entries[#entries + 1] = new_entry(SECTION.Form, node, text)
          end
        end
        for i = 2, #values do
          process(values[i])
        end
      end

      for _, child in ipairs(root:children()) do
        process(child)
      end

      return format_skeleton(entries, {}, nil, ".")
    end,
  }
end
