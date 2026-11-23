return function(U)
  local ast = require("lang.nix.ast")(U)

  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local truncated_msg = U.truncated_msg

  local FIELD_TRUNCATE_THRESHOLD = U.FIELD_TRUNCATE_THRESHOLD
  local SECTION = U.SECTION

  local get_text = ast.get_text
  local fn_signature = ast.fn_signature
  local is_imports_binding = ast.is_imports_binding
  local collect_list_paths = ast.collect_list_paths
  local segments_from_element = ast.segments_from_element
  local derivation_pname = ast.derivation_pname
  local is_import_apply = ast.is_import_apply

  local A = {}

  local function add_children(parent, nested, depth)
    local total = #nested
    local count = 0

    for _, ne in ipairs(nested) do
      if count < FIELD_TRUNCATE_THRESHOLD then
        parent.children[#parent.children + 1] = ne
        count = count + 1
      else
        break
      end
    end
    if total <= FIELD_TRUNCATE_THRESHOLD then
      return
    end
    if depth and depth <= 1 then
      parent.children[#parent.children + 1] = truncated_msg(total)
      return
    end
    for i = FIELD_TRUNCATE_THRESHOLD + 1, total do
      local ne = nested[i]
      local range
      if ne.line_start and ne.line_end then
        if ne.line_start == ne.line_end then
          range = "[" .. ne.line_start .. "]"
        else
          range = "[" .. ne.line_start .. "-" .. ne.line_end .. "]"
        end
      end
      parent.children[#parent.children + 1] = range and (ne.text .. " " .. range) or ne.text
    end
  end

  local function for_each_binding(binding_set, source, fn)
    for _, b in ipairs(binding_set:children()) do
      if ast.is_binding(b) then
        local ap = ast.binding_attrpath(b)
        local v = ast.binding_expression(b)
        if ap and v then
          fn(get_text(ap, source), v, b)
        end
      elseif ast.is_inherit(b) or ast.is_inherit_from(b) then
        for _, attr in ipairs(b:field("attrs")) do
          local text = get_text(attr, source)
          if text and text ~= "" then
            fn(text, attr, b)
          end
        end
      end
    end
  end

  -- Walks the AST and emits import entries for:
  --   1. `imports = [ ... ]` bindings (the home-manager/NixOS pattern)
  --   2. Bare `import <path>` apply expressions
  -- Recurses into the children of attrsets and let-bodies so deeply nested
  -- imports lists are found, but does not re-enter nodes it has already
  -- handled (to avoid emitting duplicates for nested `import` calls).
  A.collect_imports = function(root, source)
    local imports = {}
    local function walk(n)
      if ast.is_binding(n) and is_imports_binding(n, source) then
        local val = ast.binding_expression(n)
        if val and ast.is_list_expression(val) then
          local paths = collect_list_paths(val, source)
          if #paths > 0 then
            imports[#imports + 1] = new_import_entry(n, paths)
          end
        end
        return
      end
      if ast.is_apply_expression(n) then
        local fn_node = ast.apply_function(n)
        local arg_node = ast.apply_argument(n)
        if fn_node and arg_node and is_import_apply(n, source) then
          local segments = segments_from_element(arg_node, source)
          if segments and #segments > 0 then
            imports[#imports + 1] = new_import_entry(n, { segments })
          end
          return
        end
        if fn_node then
          walk(fn_node)
        end
        if arg_node then
          walk(arg_node)
        end
        return
      end
      for _, child in ipairs(n:children()) do
        walk(child)
      end
    end
    walk(root)
    return imports
  end

  A.dispatch_binding = function(name, val, node, source, depth)
    if name == "imports" then
      return nil
    end

    if ast.is_function_expression(val) then
      local entry = new_entry(SECTION.Function, node, fn_signature(name, val, source))
      local body = ast.body(val)
      if body then
        add_children(entry, A.collect(body, source, depth - 1), depth - 1)
      end
      return entry
    elseif
      ast.is_attrset_expression(val)
      or ast.is_rec_attrset_expression(val)
      or ast.is_let_attrset_expression(val)
    then
      local entry = new_entry(SECTION.Constant, node, name)
      add_children(entry, A.collect(val, source, depth - 1), depth - 1)
      return entry
    elseif ast.is_let_expression(val) then
      local entry = new_entry(SECTION.Constant, node, name)
      local bs = ast.find_child(val, "binding_set")
      if bs then
        for_each_binding(bs, source, function(sub_name, sub_val, sub_node)
          local sub = A.dispatch_binding(sub_name, sub_val, sub_node, source, depth - 1)
          if sub then
            add_children(entry, { sub }, depth - 1)
          end
        end)
      end
      local body = ast.body(val)
      if body then
        add_children(entry, A.collect(body, source, depth - 1), depth - 1)
      end
      return entry
    elseif ast.is_apply_expression(val) then
      if is_import_apply(val, source) then
        return nil
      end
      local pname = derivation_pname(val, source)
      local label = pname and (name .. " (" .. pname .. ")") or name
      return new_entry(SECTION.Constant, node, label)
    else
      return new_entry(SECTION.Constant, node, name)
    end
  end

  A.collect = function(node, source, depth)
    if depth <= 0 then
      return {}
    end
    local entries = {}

    if ast.is_source_code(node) then
      local expr = ast.expression(node)
      if expr then
        entries = A.collect(expr, source, depth)
      end
    elseif ast.is_function_expression(node) then
      entries[#entries + 1] = new_entry(SECTION.Function, node, fn_signature("fns", node, source))
      local body = ast.body(node)
      if body then
        for _, e in ipairs(A.collect(body, source, depth - 1)) do
          entries[#entries + 1] = e
        end
      end
    elseif
      ast.is_attrset_expression(node)
      or ast.is_rec_attrset_expression(node)
      or ast.is_let_attrset_expression(node)
    then
      local bs = ast.find_child(node, "binding_set")
      if bs then
        for_each_binding(bs, source, function(name, val, binding_node)
          local entry = A.dispatch_binding(name, val, binding_node, source, depth)
          if entry then
            entries[#entries + 1] = entry
          end
        end)
      end
    elseif ast.is_let_expression(node) then
      local bs = ast.find_child(node, "binding_set")
      if bs then
        for_each_binding(bs, source, function(name, val, binding_node)
          local entry = A.dispatch_binding(name, val, binding_node, source, depth - 1)
          if entry then
            entries[#entries + 1] = entry
          end
        end)
      end
      local body = ast.body(node)
      if body then
        for _, e in ipairs(A.collect(body, source, depth - 1)) do
          entries[#entries + 1] = e
        end
      end
    elseif ast.is_with_expression(node) then
      local body = ast.body(node)
      if body then
        entries = A.collect(body, source, depth)
      end
    elseif ast.is_parenthesized_expression(node) then
      local expr = ast.expression(node)
      if expr then
        entries = A.collect(expr, source, depth)
      end
    elseif ast.is_list_expression(node) then
      for _, el in ipairs(node:field("element")) do
        for _, e in ipairs(A.collect(el, source, depth)) do
          entries[#entries + 1] = e
        end
      end
    end

    return entries
  end

  return A
end
