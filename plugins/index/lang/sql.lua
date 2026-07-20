return function(U)
  local get_text = U.get_text
  local find_child = U.find_child
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local SECTION = U.SECTION
  local CHILD_BRIEF = U.CHILD_BRIEF
  local extract_fields_truncated = U.extract_fields_truncated

  local BODY_TRUNCATE = 80

  -- Statements we surface as "definitions". Everything else (SELECT/INSERT/
  -- UPDATE/DELETE, ALTER/DROP, transactions control, etc.) is DML/noise and
  -- gets skipped, same as how other extractors ignore usage/expression nodes.
  -- Note: tree-sitter-sequel (as published on crates.io, unlike the grammar's
  -- `main` branch) has no distinct `create_procedure` node yet -- CREATE
  -- PROCEDURE isn't parseable as DDL at all in this grammar version -- so
  -- there is intentionally no procedure handling here.
  local RECOGNIZED = {
    create_table = true,
    create_view = true,
    create_materialized_view = true,
    create_function = true,
    create_trigger = true,
    create_index = true,
    create_type = true,
    create_schema = true,
  }

  local function column_format(child, source)
    local name_node = child:field("name")[1]
    local type_node = child:field("type")[1]
    local name = name_node and get_text(name_node, source) or "?"
    local type_str = type_node and (" " .. get_text(type_node, source)) or ""
    return compact_ws(name .. type_str)
  end

  local function extract_table(node, source)
    local ref = find_child(node, "object_reference")
    local name = ref and get_text(ref, source) or "?"
    local entry = new_entry(SECTION.Class, node, "TABLE " .. name)
    local body = find_child(node, "column_definitions")
    if body then
      entry.children = extract_fields_truncated(body, source, "column_definition", column_format)
    end
    return entry
  end

  local function extract_view(node, source, materialized)
    local ref = find_child(node, "object_reference")
    local name = ref and get_text(ref, source) or "?"
    local keyword = materialized and "MATERIALIZED VIEW " or "VIEW "
    local entry = new_entry(SECTION.Type, node, keyword .. name)
    local query = find_child(node, "create_query")
    if query then
      entry.children = { truncate(compact_ws(get_text(query, source)), BODY_TRUNCATE) }
    end
    return entry
  end

  local function extract_function(node, source)
    local ref = find_child(node, "object_reference")
    local name = ref and get_text(ref, source) or "?"
    local args_node = find_child(node, "function_arguments")
    local args = args_node and get_text(args_node, source) or "()"
    local lang_node = find_child(node, "function_language")
    local lang_str = lang_node and (" " .. compact_ws(get_text(lang_node, source))) or ""
    local entry = new_entry(SECTION.Function, node, compact_ws("FUNCTION " .. name .. args .. lang_str))
    local body = find_child(node, "function_body")
    if body then
      entry.children = { truncate(compact_ws(get_text(body, source)), BODY_TRUNCATE) }
    end
    return entry
  end

  -- create_trigger has three `object_reference` children in a fixed order:
  -- the trigger name, then (after ON) the table, then (after EXECUTE
  -- FUNCTION/PROCEDURE) the callee. We only want the first two.
  local function extract_trigger(node, source)
    local refs = {}
    for _, child in ipairs(node:children()) do
      if child:type() == "object_reference" then
        refs[#refs + 1] = get_text(child, source)
      end
    end
    if #refs == 0 then
      return nil
    end
    local label = "TRIGGER " .. refs[1]
    if refs[2] then
      label = label .. " ON " .. refs[2]
    end
    return new_entry(SECTION.Function, node, label)
  end

  -- create_index's optional name is exposed under the grammar field "column"
  -- (not "name"); the indexed table is the sole `object_reference` child, and
  -- `index_fields` already renders as "(col1, col2)" via get_text. The grammar
  -- currently only yields the first identifier of a dotted name, so treat a
  -- name followed by a dot as absent and fall back to the anonymous label.
  local function extract_index(node, source)
    local name_node = node:field("column")[1]
    local name = name_node and get_text(name_node, source)
    if name then
      local _, _, end_byte = name_node:end_()
      if source:sub(end_byte + 1, end_byte + 1) == "." then
        name = nil
      end
    end
    local table_node = find_child(node, "object_reference")
    local table_name = table_node and get_text(table_node, source) or "?"
    local fields_node = find_child(node, "index_fields")
    local cols = fields_node and compact_ws(get_text(fields_node, source)) or "()"
    if name then
      return new_entry(SECTION.Function, node, "INDEX " .. name .. " ON " .. table_name .. cols)
    end
    return new_entry(SECTION.Function, node, "INDEX ON " .. table_name .. cols)
  end

  local function extract_type(node, source)
    local ref = find_child(node, "object_reference")
    local name = ref and get_text(ref, source) or "?"
    local entry = new_entry(SECTION.Type, node, "TYPE " .. name)
    local body = find_child(node, "column_definitions")
    local enum_body = find_child(node, "enum_elements")
    if body then
      entry.children = extract_fields_truncated(body, source, "column_definition", column_format)
    elseif enum_body then
      local values = {}
      for _, child in ipairs(enum_body:children()) do
        if child:type() == "literal" then
          values[#values + 1] = get_text(child, source)
        end
      end
      if #values > 0 then
        entry.children = values
        entry.child_kind = CHILD_BRIEF
      end
    end
    return entry
  end

  local function extract_schema(node, source)
    local id = find_child(node, "identifier")
    local name = id and get_text(id, source) or "?"
    return new_entry(SECTION.Module, node, name)
  end

  local extract_nodes

  local function dispatch(kind, node, source, attrs)
    if kind == "create_table" then
      return { extract_table(node, source) }
    elseif kind == "create_view" then
      return { extract_view(node, source, false) }
    elseif kind == "create_materialized_view" then
      return { extract_view(node, source, true) }
    elseif kind == "create_function" then
      return { extract_function(node, source) }
    elseif kind == "create_trigger" then
      local e = extract_trigger(node, source)
      return e and { e } or {}
    elseif kind == "create_index" then
      return { extract_index(node, source) }
    elseif kind == "create_type" then
      return { extract_type(node, source) }
    elseif kind == "create_schema" then
      return { extract_schema(node, source) }
    end
    return {}
  end

  -- `program`/`block`/`transaction` all wrap their DDL/DML content in a
  -- `statement` node (the grammar's `_ddl_statement`/`_dml_write`/`_dml_read`
  -- choices are hidden rules, so their expansion is spliced directly into
  -- `statement`'s children). Unwrap one level to find the real definition,
  -- and recurse into `block`/`transaction` bodies (BEGIN...END / BEGIN...COMMIT)
  -- so wrapped DDL still gets indexed.
  extract_nodes = function(node, source, attrs)
    local kind = node:type()
    if RECOGNIZED[kind] then
      return dispatch(kind, node, source, attrs)
    elseif kind == "statement" then
      for _, child in ipairs(node:children()) do
        if RECOGNIZED[child:type()] then
          return dispatch(child:type(), child, source, attrs)
        end
      end
      return {}
    elseif kind == "block" or kind == "transaction" then
      local entries = {}
      for _, child in ipairs(node:children()) do
        if child:type() == "statement" then
          for _, e in ipairs(extract_nodes(child, source, attrs)) do
            entries[#entries + 1] = e
          end
        end
      end
      return entries
    end
    return {}
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, _source)
      local kind = node:type()
      return kind == "comment" or kind == "marginalia"
    end,

    extract_nodes = extract_nodes,
  }
end
