return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local truncate = U.truncate
  local new_entry = U.new_entry
  local simple_import = U.simple_import
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION

  local MEMBER_TRUNCATE = 60
  local SIGNATURE_TRUNCATE = 100
  local USE_PREFIXES = { "use%s+" }

  local DECLARATION_KINDS = {
    signal_declaration = "",
    constant_declaration = "constant ",
    variable_declaration = "variable ",
    shared_variable_declaration = "shared variable ",
  }

  local function field_text(node, field, source)
    local value = node:field(field)[1]
    return value and get_text(value, source) or nil
  end

  local function one_line(node, source)
    return compact_ws(get_text(node, source))
  end

  local function statement_line(node, source)
    return (one_line(node, source):gsub("%s*;$", ""))
  end

  -- Labelled constructs (`b1 : block`, `p : process`, `g : for ... generate`)
  -- keep the label as the first child.
  local function label_of(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "label_declaration" then
        local label = child:named_child(0)
        return label and get_text(label, source) or nil
      end
    end
    return nil
  end

  local function labelled(label, text)
    return label and (label .. " : " .. text) or text
  end

  local function declared_names(node, source)
    local names = {}
    for _, child in ipairs(node:named_children()) do
      if child:type() == "identifier_list" then
        for _, ident in ipairs(child:named_children()) do
          names[#names + 1] = get_text(ident, source)
        end
      end
    end
    return names
  end

  local function declared_type(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "subtype_indication" then
        return truncate(one_line(child, source), MEMBER_TRUNCATE)
      end
    end
    return nil
  end

  local function declaration_line(node, source)
    local kind = node:type()
    local names = declared_names(node, source)
    if #names == 0 then
      return nil
    end
    local decl_type = declared_type(node, source)
    return truncate(
      DECLARATION_KINDS[kind] .. table.concat(names, ", ") .. (decl_type and (" : " .. decl_type) or ""),
      MEMBER_TRUNCATE
    )
  end

  -- `type state_t is (IDLE, RUN)`; the right side of a full definition spills
  -- past the declaration otherwise.
  local function type_line(node, source)
    local name = field_text(node, "type", source)
    if not name then
      return nil
    end
    local body = nil
    for _, child in ipairs(node:named_children()) do
      if child:type() ~= "identifier" then
        body = one_line(child, source)
        break
      end
    end
    local is_subtype = node:type() == "subtype_declaration"
    local separator = body and (is_subtype and " " or " is ") or ""
    return truncate((is_subtype and "subtype " or "type ") .. name .. separator .. (body or ""), MEMBER_TRUNCATE)
  end

  local function attribute_line(node, source)
    if node:type() == "attribute_declaration" then
      local name = field_text(node, "attribute", source)
      local attr_type = field_text(node, "type", source)
      return name and truncate("attribute " .. name .. (attr_type and (" : " .. attr_type) or ""), MEMBER_TRUNCATE)
        or nil
    end
    return truncate(statement_line(node, source), MEMBER_TRUNCATE)
  end

  local function add_all(dst, src)
    for _, item in ipairs(src) do
      dst[#dst + 1] = item
    end
  end

  local extract_nodes
  local function concurrent_entries(node, source)
    local entries = {}
    for _, child in ipairs(node:named_children()) do
      add_all(entries, extract_nodes(child, source))
    end
    return entries
  end

  -- Ports and generics are `interface_declaration` nodes whose direction and
  -- type sit under `simple_mode_indication`.
  local function interface_line(decl, source)
    local names = declared_names(decl, source)
    if #names == 0 then
      return nil
    end
    local shape = nil
    for _, child in ipairs(decl:named_children()) do
      if child:type() == "simple_mode_indication" then
        shape = one_line(child, source)
      end
    end
    return truncate(table.concat(names, ", ") .. (shape and (" : " .. shape) or ""), MEMBER_TRUNCATE)
  end

  local function interface_lines(node, source)
    local lines = {}
    for _, clause in ipairs(node:named_children()) do
      local kind = clause:type()
      if kind == "port_clause" or kind == "generic_clause" then
        for _, list in ipairs(clause:named_children()) do
          for _, decl in ipairs(list:named_children()) do
            if decl:type() == "interface_declaration" then
              local line = interface_line(decl, source)
              if line then
                lines[#lines + 1] = line
              end
            end
          end
        end
      end
    end
    return lines
  end

  local function subprogram_entry(node, source)
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "function_specification" or kind == "procedure_specification" then
        local name = field_text(child, kind == "function_specification" and "function" or "procedure", source)
        if name then
          return new_entry(SECTION.Function, node, truncate(one_line(child, source), SIGNATURE_TRUNCATE))
        end
      end
    end
    return nil
  end

  -- Declarative parts hold whichever of these a design unit is allowed. Each
  -- becomes its own entry so a package body or nested block can surface it.
  local function declaration_entries(node, source, skip_subprograms)
    local entries = {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if DECLARATION_KINDS[kind] then
        local line = declaration_line(child, source)
        if line then
          entries[#entries + 1] = new_entry(SECTION.Constant, child, line)
        end
      elseif kind == "type_declaration" or kind == "subtype_declaration" then
        local line = type_line(child, source)
        if line then
          entries[#entries + 1] = new_entry(SECTION.Type, child, line)
        end
      elseif kind == "component_declaration" then
        local name = field_text(child, "component", source)
        if name then
          local entry = new_entry(SECTION.Type, child, truncate("component " .. name, MEMBER_TRUNCATE))
          for _, body in ipairs(child:named_children()) do
            if body:type() == "component_body" then
              add_all(entry.children, interface_lines(body, source))
            end
          end
          entries[#entries + 1] = entry
        end
      elseif kind == "attribute_declaration" or kind == "attribute_specification" then
        local line = attribute_line(child, source)
        if line then
          entries[#entries + 1] = new_entry(SECTION.Constant, child, line)
        end
      elseif kind == "subprogram_declaration" and not skip_subprograms then
        local entry = subprogram_entry(child, source)
        if entry then
          entries[#entries + 1] = entry
        end
      end
    end
    return entries
  end

  local function attach_lines(entry, entries)
    for _, child in ipairs(entries) do
      entry.children[#entry.children + 1] = child
    end
    return entries
  end

  local function extract_entity(node, source)
    local name = field_text(node, "entity", source)
    if not name then
      return nil
    end
    local entry = new_entry(SECTION.Type, node, "entity " .. name)
    for _, head in ipairs(node:named_children()) do
      if head:type() == "entity_head" then
        add_all(entry.children, interface_lines(head, source))
      end
    end
    return entry
  end

  local function extract_architecture(node, source)
    local name = field_text(node, "architecture", source)
    local entity = field_text(node, "entity", source)
    if not name then
      return nil
    end
    local entry = new_entry(SECTION.Impl, node, "architecture " .. name .. (entity and (" of " .. entity) or ""))
    local entries = { entry }
    for _, head in ipairs(node:named_children()) do
      local kind = head:type()
      if kind == "architecture_head" then
        attach_lines(entry, declaration_entries(head, source))
      elseif kind == "concurrent_block" then
        add_all(entries, concurrent_entries(head, source))
      end
    end
    return entries
  end

  local function extract_package(node, source)
    local name = field_text(node, "package", source)
    return name and new_entry(SECTION.Module, node, "package " .. name) or nil
  end

  local function component_instantiation(node, source)
    local label = label_of(node, source)
    local unit = field_text(node, "component", source)
    if not unit then
      for _, child in ipairs(node:named_children()) do
        if child:type() == "instantiated_unit" then
          unit = one_line(child, source)
        end
      end
    end
    if not unit then
      return nil
    end
    return truncate(labelled(label, "instance " .. unit), MEMBER_TRUNCATE)
  end

  local function process_entry(node, source)
    local label = label_of(node, source)
    return new_entry(SECTION.Function, node, truncate(labelled(label, "process"), SIGNATURE_TRUNCATE))
  end

  local function generate_label(node, source)
    local label = label_of(node, source)
    local keyword = node:type() == "for_generate_statement" and "for generate" or "if generate"
    return labelled(label, keyword)
  end

  local function extract_configuration(node, source)
    local name = field_text(node, "configuration", source)
    local entity = field_text(node, "entity", source)
    if not name then
      return nil
    end
    return new_entry(SECTION.Block, node, "configuration " .. name .. (entity and (" of " .. entity) or ""))
  end

  -- A block or generate body declares in its own head and nests its
  -- statements one level down, so both are read. Declarations stay under the
  -- container: they are scoped to it, not to the enclosing design unit.
  local function with_concurrent(entry, node, source)
    local entries = { entry }
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "block_head" or kind == "generate_body" then
        attach_lines(entry, declaration_entries(child, source))
      end
    end
    for _, child in ipairs(node:named_children()) do
      add_all(entries, concurrent_entries(child, source))
    end
    return entries
  end

  local function extract_block(node, source)
    local label = label_of(node, source)
    local entry = new_entry(SECTION.Block, node, truncate(labelled(label, "block"), SIGNATURE_TRUNCATE))
    return with_concurrent(entry, node, source)
  end

  local function extract_generate(node, source)
    local entry = new_entry(SECTION.Block, node, truncate(generate_label(node, source), SIGNATURE_TRUNCATE))
    return with_concurrent(entry, node, source)
  end

  -- A body contributes only its contents; the header belongs to the
  -- declaration, so a package declaration plus its body stays one entry.
  local function package_entries(node, source)
    local entries = {}
    if node:type() == "package_declaration" then
      local entry = extract_package(node, source)
      if entry then
        entries[1] = entry
      end
    end
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "package_definition_body" or kind == "package_declaration_body" then
        add_all(entries, declaration_entries(child, source, true))
        for _, sub in ipairs(child:named_children()) do
          add_all(entries, extract_nodes(sub, source))
        end
      end
    end
    return entries
  end

  extract_nodes = function(node, source, _attrs)
    local kind = node:type()
    if kind == "design_unit" then
      local entries = {}
      for _, child in ipairs(node:named_children()) do
        add_all(entries, extract_nodes(child, source))
      end
      return entries
    elseif kind == "use_clause" then
      local entry = simple_import(node, source, USE_PREFIXES, ".")
      return entry and { entry } or {}
    elseif kind == "library_clause" then
      local names = {}
      for _, list in ipairs(node:named_children()) do
        for _, name in ipairs(list:named_children()) do
          names[#names + 1] = { get_text(name, source) }
        end
      end
      return #names > 0 and { new_import_entry(node, names, "library") } or {}
    elseif kind == "entity_declaration" then
      local entry = extract_entity(node, source)
      return entry and { entry } or {}
    elseif kind == "architecture_definition" then
      return extract_architecture(node, source)
    elseif kind == "package_declaration" or kind == "package_definition" then
      return package_entries(node, source)
    elseif kind == "subprogram_definition" or kind == "subprogram_declaration" then
      local entry = subprogram_entry(node, source)
      return entry and { entry } or {}
    elseif kind == "process_statement" then
      return { process_entry(node, source) }
    elseif kind == "block_statement" then
      return extract_block(node, source)
    elseif kind == "for_generate_statement" or kind == "if_generate_statement" then
      return extract_generate(node, source)
    elseif kind == "component_instantiation_statement" then
      local line = component_instantiation(node, source)
      return line and { new_entry(SECTION.Impl, node, line) } or {}
    elseif kind == "configuration_declaration" then
      local entry = extract_configuration(node, source)
      return entry and { entry } or {}
    end
    return {}
  end

  return {
    import_separator = ".",

    is_doc_comment = function(node, _source)
      return node:type() == "comment"
    end,

    extract_nodes = extract_nodes,
  }
end
