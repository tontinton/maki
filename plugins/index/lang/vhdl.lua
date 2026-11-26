return function(U)
  local get_text = U.get_text
  local one_line = U.one_line
  local statement_line = U.statement_line
  local field_text = U.field_text
  local add_all = U.add_all
  local truncate = U.truncate
  local new_entry = U.new_entry
  local ranged = U.ranged
  local format_range = U.format_range
  local simple_import = U.simple_import
  local new_import_entry = U.new_import_entry
  local SECTION = U.SECTION

  local PARAM_TRUNCATE = 16
  -- A package exports its members, so a hand-written one must list whole; the
  -- budget holds per container and per section, which keeps a generated file's
  -- contents bounded without hiding a small package's.
  local PACKAGE_MEMBER_CAP = 32
  local PORT_TRUNCATE = 64
  -- A component declaration is a file-local copy of an entity interface, and a
  -- component-style instantiation takes its un-mapped formals from here, so the
  -- block is not redundant. It is still the same interface twice, so it gets
  -- the smaller budget: 32 keeps mid-size packages whole and truncates the IP
  -- wrappers whose list runs to hundreds.
  local COMPONENT_CAP = 32
  local ATTRIBUTE_CAP = 8
  local MEMBER_TRUNCATE = 60
  local SIGNATURE_TRUNCATE = 100
  -- A record is usually a packet or a register map, so its field list is the
  -- content a reader came for rather than detail to trim.
  local RECORD_FIELD_CAP = 24
  -- A signal is the VHDL net and a constant its parameter, so they are separate
  -- kinds with a budget each.
  local SIGNAL_SECTION = "signal"
  -- A declaration's section is its kind's unless the kind says otherwise, and
  -- constants are the default a declarative part holds most of.
  local SECTION_OF = {
    signal_declaration = SIGNAL_SECTION,
    interface_subprogram_declaration = SECTION.Function,
    interface_type_declaration = SECTION.Type,
  }

  -- A container holds several kinds, so a cut says which kind it cut. The
  -- names match the sections a reader already sees.
  local MEMBER_KIND = {
    [SECTION.Constant] = "consts",
    [SIGNAL_SECTION] = "nets",
    [SECTION.Type] = "types",
    [SECTION.DesignUnit] = "units",
    [SECTION.Function] = "fns",
    ["attribute"] = "attrs",
  }
  local function kind_marker(cap, name)
    return function(key, total)
      return "[" .. (total - cap) .. " " .. (name or MEMBER_KIND[key] or "items") .. " truncated]"
    end
  end
  -- Entry nesting is shallow in practice (a generated corpus tops out at 5), so
  -- this exists to stop pathological input from exploding the walk, not to trim
  -- real designs. At the limit a construct keeps its line and range, no children.
  local MAX_DEPTH = 20
  local USE_PREFIXES = { "use%s+" }
  local CONTEXT_PREFIXES = { "context%s+" }

  local DECLARATION_KINDS = {
    signal_declaration = "",
    constant_declaration = "constant ",
    variable_declaration = "variable ",
    shared_variable_declaration = "shared variable ",
    element_declaration = "",
  }

  -- A port or generic names its direction and type under `simple_mode_indication`;
  -- the kind carries any `constant`/`signal`/`variable` mode, which the port
  -- syntax allows but `simple_mode_indication` does not repeat.
  local INTERFACE_DECLARATION = {
    interface_declaration = "",
    interface_constant_declaration = "constant ",
    interface_signal_declaration = "signal ",
    interface_variable_declaration = "variable ",
  }

  -- A type or subprogram generic is a parameter slot, not a declaration, so it
  -- says so rather than reading like a type or subprogram the unit declares.
  local INTERFACE_STATEMENT = {
    interface_type_declaration = { head = "generic type ", strip = "^type%s+" },
    interface_subprogram_declaration = { head = "generic " },
  }

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
      local kind = child:type()
      if kind == "record_type_definition" then
        -- The members are listed under the entry, so the header stops at the
        -- keyword instead of repeating them on one long line.
        body = "record"
        break
      elseif kind == "enumeration_type_definition" then
        -- Same reason as a record: the literals are listed under the entry, so
        -- a wide enumeration is not cut mid-name on the header line.
        body = "enum"
        break
      elseif kind == "protected_type_declaration" then
        body = "protected"
        break
      elseif kind == "protected_type_body" then
        body = "protected body"
        break
      elseif kind ~= "identifier" then
        body = one_line(child, source)
        break
      end
    end
    local is_subtype = node:type() == "subtype_declaration"
    local separator = body and (is_subtype and " " or " is ") or ""
    return truncate((is_subtype and "subtype " or "type ") .. name .. separator .. (body or ""), MEMBER_TRUNCATE)
  end

  -- A single-key bucket: `capped_buckets` counts every item it is given, so the
  -- total is known without keeping the ones past the threshold.
  local KEY = "all"

  -- A record's fields are `element_declaration` siblings under
  -- `record_type_definition`, so the type line names the record and these list
  -- what it holds.
  local function record_fields(node, source)
    local fields = U.capped_buckets(RECORD_FIELD_CAP)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "record_type_definition" then
        for _, element in ipairs(child:named_children()) do
          if element:type() == "element_declaration" then
            local line = declaration_line(element, source)
            if line then
              fields.push(KEY, line)
            end
          end
        end
      end
    end
    return fields.get(KEY, kind_marker(RECORD_FIELD_CAP, "fields"))
  end

  -- An enumeration's literals are the whole interface of the type, and the only
  -- place those names appear, so they are listed like a record's fields.
  local function enum_literals(node, source)
    local literals = {}
    for _, child in ipairs(node:named_children()) do
      if child:type() == "enumeration_type_definition" then
        for _, literal in ipairs(child:named_children()) do
          if literal:type() == "enumeration_literal" then
            literals[#literals + 1] = get_text(literal, source)
          end
        end
      end
    end
    return literals
  end

  -- Synthesis and CDC intent an agent has to know about; everything else a file
  -- carries is vendor packaging metadata (Xilinx fills a whole IP with
  -- `soft_hlutnm`, `xram_*` and `X_INTERFACE_*`), which says nothing about the
  -- design. Same rule Rust applies by keeping only `derive` and `cfg`.
  local ATTR_KEEP = {
    async_reg = true,
    async_reg_boolean = true,
    dont_touch = true,
    keep = true,
    keep_hierarchy = true,
    mark_debug = true,
    parallel_case = true,
    full_case = true,
    ram_style = true,
    rom_style = true,
    rtl_keep = true,
    translate_off = true,
    translate_on = true,
  }

  local function attribute_name(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "attribute_identifier" then
        return get_text(child, source):lower()
      end
    end
    return nil
  end

  -- A forward declaration names a type and nothing else, so it is dropped: the
  -- specification that follows says what the attribute is and what it is on.
  local function attribute_line(node, source)
    if node:type() == "attribute_declaration" then
      return nil
    end
    local name = attribute_name(node, source)
    if not name or not ATTR_KEEP[name] then
      return nil
    end
    return truncate(statement_line(node, source), MEMBER_TRUNCATE)
  end

  local extract_nodes
  -- A declarative part is attached by the construct that owns it, so walking it
  -- here would list each of its declarations a second time.
  local DECLARATIVE_PART = {
    block_head = true,
    generate_head = true,
    architecture_head = true,
    process_head = true,
    entity_head = true,
  }

  local function concurrent_entries(node, source, depth)
    local entries = {}
    if DECLARATIVE_PART[node:type()] then
      return entries
    end
    for _, child in ipairs(node:named_children()) do
      if not DECLARATIVE_PART[child:type()] then
        add_all(entries, extract_nodes(child, source, nil, depth))
      end
    end
    return entries
  end

  local function interface_line(decl, source)
    local kind = decl:type()
    local statement = INTERFACE_STATEMENT[kind]
    if statement then
      local text = statement_line(decl, source)
      if statement.strip then
        text = text:gsub(statement.strip, "")
      end
      -- A subprogram generic is a signature, so it gets the room a signature gets.
      local limit = kind == "interface_subprogram_declaration" and SIGNATURE_TRUNCATE or MEMBER_TRUNCATE
      return truncate(statement.head .. text, limit)
    end
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
    local prefix = INTERFACE_DECLARATION[kind] or ""
    return truncate(prefix .. table.concat(names, ", ") .. (shape and (" : " .. shape) or ""), MEMBER_TRUNCATE)
  end

  -- A unit's ports and its generics are separate budgets: the ports are the
  -- interface and matter more, so a long generic list cannot cut them off.
  local INTERFACE_CAP = { port_clause = PORT_TRUNCATE, generic_clause = PARAM_TRUNCATE }
  local COMPONENT_INTERFACE_CAP = { port_clause = COMPONENT_CAP, generic_clause = COMPONENT_CAP }

  local function interface_lines(node, source, caps)
    local buckets = {}
    for _, clause in ipairs(node:named_children()) do
      local kind = clause:type()
      local cap = caps[kind]
      if cap then
        local lines = buckets[kind] or U.capped_buckets(cap)
        buckets[kind] = lines
        for _, list in ipairs(clause:named_children()) do
          for _, decl in ipairs(list:named_children()) do
            local item = decl:type()
            if INTERFACE_DECLARATION[item] ~= nil or INTERFACE_STATEMENT[item] then
              local line = interface_line(decl, source)
              if line then
                lines.push("all", ranged(line, format_range(U.line_start(decl), U.line_end(decl))))
              end
            end
          end
        end
      end
    end
    -- Each clause has its own budget, so its marker is built from that budget.
    local INTERFACE_NAME = { generic_clause = "generics", port_clause = "ports" }
    local result = {}
    for _, kind in ipairs({ "generic_clause", "port_clause" }) do
      local bucket = buckets[kind]
      add_all(result, bucket and bucket.get("all", kind_marker(caps[kind], INTERFACE_NAME[kind])) or {})
    end
    return result
  end

  local SUBPROGRAM_NESTED = { subprogram_definition = true, subprogram_declaration = true }

  local subprogram_entry

  local case_headers

  subprogram_entry = function(node, source)
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "function_specification" or kind == "procedure_specification" then
        local name = field_text(child, kind == "function_specification" and "function" or "procedure", source)
        if name then
          local entry = new_entry(SECTION.Function, node, truncate(one_line(child, source), SIGNATURE_TRUNCATE))
          -- A subprogram declared in the head is scoped to this one, so it
          -- follows its owner rather than standing beside it.
          for _, head in ipairs(node:named_children()) do
            if head:type() == "subprogram_head" then
              for _, inner in ipairs(head:named_children()) do
                if SUBPROGRAM_NESTED[inner:type()] then
                  local nested = subprogram_entry(inner, source)
                  if nested then
                    entry.children[#entry.children + 1] = nested
                  end
                end
              end
            end
          end
          add_all(entry.children, case_headers(node, source))
          return entry
        end
      end
    end
    return nil
  end

  -- A protected type declares the subprograms its instances accept; the
  -- declarations hang off `protected_type_declaration` as one line each.
  local PROTECTED_PART = { protected_type_declaration = true, protected_type_body = true }
  local PROTECTED_METHOD = { subprogram_declaration = true, subprogram_definition = true }

  local function protected_methods(node, source)
    local methods = U.capped_buckets()
    for _, child in ipairs(node:named_children()) do
      if PROTECTED_PART[child:type()] then
        for _, decl in ipairs(child:named_children()) do
          if PROTECTED_METHOD[decl:type()] then
            local entry = subprogram_entry(decl, source)
            if entry then
              methods.push(KEY, entry)
            end
          end
        end
      end
    end
    return methods.get(KEY, kind_marker(U.FIELD_TRUNCATE_THRESHOLD, "fns"))
  end

  -- Attributes are a property of the declarations around them rather than
  -- members of their own, so they keep a separate budget and their own bucket:
  -- a signal-heavy body must not starve them, and a generated file that
  -- annotates every signal must not push out the body's contents. Eight keeps
  -- every hand-written design whole and truncates only IP wrappers.
  local ATTRIBUTE_SECTION = "attribute"

  -- Declarative parts hold whichever of these a design unit is allowed. Each
  -- becomes its own entry so a package body or nested block can surface it.
  local function declaration_entries(node, source, skip_subprograms)
    local entries = {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if DECLARATION_KINDS[kind] then
        local line = declaration_line(child, source)
        if line then
          entries[#entries + 1] = new_entry(SECTION_OF[kind] or SECTION.Constant, child, line)
        end
      elseif kind == "type_declaration" or kind == "subtype_declaration" then
        local line = type_line(child, source)
        if line then
          local entry = new_entry(SECTION.Type, child, line)
          add_all(entry.children, record_fields(child, source))
          add_all(entry.children, enum_literals(child, source))
          add_all(entry.children, protected_methods(child, source))
          entries[#entries + 1] = entry
        end
      elseif kind == "component_declaration" then
        local name = field_text(child, "component", source)
        if name then
          local entry = new_entry(SECTION.DesignUnit, child, truncate("component " .. name, MEMBER_TRUNCATE))
          for _, body in ipairs(child:named_children()) do
            if body:type() == "component_body" then
              add_all(entry.children, interface_lines(body, source, COMPONENT_INTERFACE_CAP))
            end
          end
          entries[#entries + 1] = entry
        end
      elseif kind == "attribute_declaration" or kind == "attribute_specification" then
        local line = attribute_line(child, source)
        if line then
          entries[#entries + 1] = new_entry(ATTRIBUTE_SECTION, child, line)
        end
      elseif (kind == "subprogram_declaration" or kind == "subprogram_definition") and not skip_subprograms then
        local entry = subprogram_entry(child, source)
        if entry then
          entries[#entries + 1] = entry
        end
      elseif INTERFACE_DECLARATION[kind] ~= nil or INTERFACE_STATEMENT[kind] then
        local line = interface_line(child, source)
        if line then
          entries[#entries + 1] = new_entry(SECTION_OF[kind] or SECTION.Constant, child, line)
        end
      elseif kind == "alias_declaration" or kind == "group_template_declaration" or kind == "group_declaration" then
        entries[#entries + 1] = new_entry(SECTION.Type, child, truncate(statement_line(child, source), MEMBER_TRUNCATE))
      elseif kind == "package_header" then
        -- A generic package states its formals in the header, which the body
        -- wraps in a `generic_clause`/`interface_list` pair.
        add_all(entries, extract_nodes(child, source))
      end
    end
    return entries
  end

  -- Signals, types and subprograms each get their own budget, so a signal-heavy
  -- declarative part cannot starve the behaviour. Instances stay uncapped: a
  -- unit holds few by nature and an entry list has no marker to announce a cut.
  local UNCAPPED_SECTIONS = { [SECTION.Impl] = true, [SECTION.Block] = true }

  -- A truncation marker is a bare string, so it sorts after its members.
  local function by_source_line(a, b)
    return (type(a) == "table" and a.line_start or math.huge) < (type(b) == "table" and b.line_start or math.huge)
  end

  local MEMBER_ORDER = {
    SIGNAL_SECTION,
    SECTION.Constant,
    SECTION.Type,
    SECTION.DesignUnit,
    ATTRIBUTE_SECTION,
    SECTION.Function,
  }

  local function attach_lines(entry, entries, member_cap)
    local buckets = U.capped_buckets(member_cap)
    local attributes = U.capped_buckets(ATTRIBUTE_CAP)
    local uncapped = {}
    for _, child in ipairs(entries) do
      if child.section == ATTRIBUTE_SECTION then
        attributes.push(ATTRIBUTE_SECTION, child)
      elseif UNCAPPED_SECTIONS[child.section] then
        uncapped[#uncapped + 1] = child
      else
        buckets.push(child.section, child)
      end
    end
    local marker = kind_marker(member_cap or U.FIELD_TRUNCATE_THRESHOLD)
    local lists = { attributes.get(ATTRIBUTE_SECTION, marker) }
    for _, key in ipairs(MEMBER_ORDER) do
      if key ~= ATTRIBUTE_SECTION then
        lists[#lists + 1] = buckets.get(key, marker)
      end
    end
    -- The buckets hold the budget per section; merging them back by line keeps
    -- the members in the order the file declares them.
    -- Instances stay uncapped but join the merge, so they keep their place
    -- among the members rather than trailing them.
    local merged = {}
    for _, list in ipairs(lists) do
      add_all(merged, list)
    end
    add_all(merged, uncapped)
    table.sort(merged, by_source_line)
    add_all(entry.children, merged)
    return entries
  end

  local function extract_entity(node, source)
    local name = field_text(node, "entity", source)
    if not name then
      return nil
    end
    -- A design unit, not `mod:`: `mod:` joins entries into a bare name list and
    -- drops children, which would lose the port list.
    local entry = new_entry(SECTION.DesignUnit, node, "entity " .. name)
    for _, head in ipairs(node:named_children()) do
      if head:type() == "entity_head" then
        add_all(entry.children, interface_lines(head, source, INTERFACE_CAP))
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
    local concurrent = {}
    for _, head in ipairs(node:named_children()) do
      local kind = head:type()
      if kind == "architecture_head" then
        attach_lines(entry, declaration_entries(head, source))
      elseif kind == "concurrent_block" then
        add_all(concurrent, concurrent_entries(head, source, 1))
      end
    end
    attach_lines(entry, concurrent)
    return { entry }
  end

  -- A labelled `u1 : sub;` is grammatically a procedure call, not an
  -- instantiation: only symbol resolution tells them apart, so the label is the
  -- signal. An unlabelled call stays dropped, the way an unlabelled `assign` is.
  local function component_instantiation(node, source)
    local label = label_of(node, source)
    local unit = field_text(node, "component", source)
    if not unit then
      for _, child in ipairs(node:named_children()) do
        local kind = child:type()
        if kind == "instantiated_unit" or (kind == "name" and label) then
          unit = one_line(child, source)
        end
      end
    end
    if not unit then
      return nil
    end
    return truncate(labelled(label, "instance " .. unit), MEMBER_TRUNCATE)
  end

  -- The maps carry what this instance is configured with, which no other entry
  -- states: a component declaration supplies only the defaults. The bindings
  -- stay in the source, so each map is one labelled line with its own range.
  local MAP_LABEL = { generic_map_aspect = "generic map", port_map_aspect = "port map" }

  local function instantiation_entry(node, source)
    local line = component_instantiation(node, source)
    if not line then
      return {}
    end
    local entry = new_entry(SECTION.Impl, node, line)
    for _, child in ipairs(node:named_children()) do
      local label = MAP_LABEL[child:type()]
      if label then
        entry.children[#entry.children + 1] = ranged(label, format_range(U.line_start(child), U.line_end(child)))
      end
    end
    return { entry }
  end

  -- The sensitivity list is what tells a synchronous process from one with an
  -- asynchronous reset, so it belongs on the line. `(all)` is a list of its
  -- own; a process driven only by a `wait` has none.
  local function sensitivity_text(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "sensitivity_specification" then
        return " " .. truncate(one_line(child, source), SIGNATURE_TRUNCATE)
      end
    end
    return ""
  end

  -- A case statement is where a state machine or a decoder lives. Only the
  -- header is listed: a branch names nothing without the assignment it holds.
  -- `if` is left alone because nearly every process has one for its clock edge.
  -- Only wrappers a sequential statement can sit inside; a nested subprogram
  -- body is reached by its own entry rather than from here.
  local PROCEDURAL_BODY = {
    sequential_block = true,
    if_statement_block = true,
    if_statement = true,
    if_statement_body = true,
    else_statement = true,
    elsif_statement = true,
    case_statement = true,
    case_body = true,
    case_statement_alternative = true,
    loop_statement = true,
    loop_body = true,
    for_loop = true,
    while_loop = true,
  }

  -- The grammar's `case_expression` spans the keyword and the parentheses, so
  -- only the spacing before an opening parenthesis needs regularising: `case(x)`
  -- and `case (x)` are the same statement.
  local function case_text(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "case_expression" then
        return truncate(one_line(child, source):gsub("^(case)%(", "%1 ("), SIGNATURE_TRUNCATE)
      end
    end
    return "case"
  end

  -- Pre-order, so a case prints where it sits in the source rather than after
  -- every shallower case in the body.
  case_headers = function(node, source, headers)
    headers = headers or {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "case_statement" then
        headers[#headers + 1] = new_entry(SECTION.Block, child, case_text(child, source))
      end
      if PROCEDURAL_BODY[kind] then
        case_headers(child, source, headers)
      end
    end
    return headers
  end

  -- A process holds behaviour, like an `always` or `initial`, so it belongs with
  -- the blocks rather than the subprograms.
  local function process_entry(node, source)
    local text = "process" .. sensitivity_text(node, source)
    local label = label_of(node, source)
    if label then
      text = labelled(label, text)
    end
    local entry = new_entry(SECTION.Block, node, truncate(text, SIGNATURE_TRUNCATE))
    add_all(entry.children, case_headers(node, source))
    return entry
  end

  -- A process head may declare subprograms the body calls; they are scoped to
  -- the process, so they follow its entry rather than standing alone. Local
  -- variables and constants are not indexed: they are the process's own state,
  -- not structure another file can reference.
  local function process_entries(node, source)
    local entry = process_entry(node, source)
    local entries = { entry }
    for _, child in ipairs(node:named_children()) do
      if child:type() == "process_head" then
        for _, decl in ipairs(child:named_children()) do
          if decl:type() == "subprogram_definition" or decl:type() == "subprogram_declaration" then
            local sub = subprogram_entry(decl, source)
            if sub then
              entries[#entries + 1] = sub
            end
          end
        end
      end
    end
    return entries
  end

  -- Generate wrappers that hold concurrent statements but are not themselves
  -- an entry; without these the body of a generate is never walked. A case
  -- alternative is NOT here: it is the only thing that says which configuration
  -- an instance belongs to, so it gets an entry of its own. A `generate_head`
  -- is not here either: it holds declarations, which the construct attaches
  -- through `declarative_head`, so walking it lists them a second time.
  local CONCURRENT_CONTAINER = {
    generate_body = true,
    case_generate_block = true,
    case_generate_body = true,
    generate_block = true,
    else_generate = true,
  }

  local GENERATE_KEYWORD = {
    for_generate_statement = "for generate",
    if_generate_statement = "if generate",
    case_generate_statement = "case generate",
  }

  local function generate_label(node, source)
    return labelled(label_of(node, source), GENERATE_KEYWORD[node:type()] or "generate")
  end

  local function extract_configuration(node, source)
    local name = field_text(node, "configuration", source)
    local entity = field_text(node, "entity", source)
    if not name then
      return nil
    end
    -- A configuration is a design unit, not a concurrent statement, so it
    -- sits with the entities rather than in `blocks:`.
    return new_entry(SECTION.DesignUnit, node, "configuration " .. name .. (entity and (" of " .. entity) or ""))
  end

  -- A context groups library and use clauses the way a design unit's head
  -- does, so its body is walked with the same arms.
  local function context_entries(node, source)
    local name = nil
    local entries = {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "identifier" and not name then
        name = get_text(child, source)
      elseif kind == "context_declaration_body" then
        for _, clause in ipairs(child:named_children()) do
          add_all(entries, extract_nodes(clause, source))
        end
      end
    end
    table.insert(entries, 1, new_entry(SECTION.DesignUnit, node, truncate("context " .. (name or ""), MEMBER_TRUNCATE)))
    return entries
  end

  -- A case alternative is identified only by its choice, which the grammar
  -- splits into a `simple_expression`, a `simple_range`, or an `others` token.
  local function alternative_text(node, source)
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "simple_expression" or kind == "simple_range" then
        return "when " .. get_text(child, source)
      end
    end
    for _, child in ipairs(node:children()) do
      if child:type() == "OTHERS" then
        return "when others"
      end
    end
    return "when"
  end

  -- Wrappers the declarative-part search descends through to reach a head. A
  -- `for` generate hides its head behind `generate_body`, an `if` generate
  -- behind `if_generate` and then `generate_body`, and a case alternative
  -- behind `case_generate_body`. These are shells, not entries, so they are
  -- kept apart from CONCURRENT_CONTAINER, which decides what gets walked.
  local HEAD_WRAPPER = {
    generate_body = true,
    generate_head = true,
    case_generate_block = true,
    case_generate_body = true,
    generate_block = true,
    else_generate = true,
    if_generate = true,
  }

  -- The declarative part of a construct hangs off a head node, but how deep it
  -- sits depends on the construct: `block_head` is a child, a `for` generate's
  -- head is two down, and an `if` generate's is three. Search for it rather than
  -- encode a depth per construct. Only shells are entered: a nested generate is
  -- a construct of its own, so its head holds its declarations, not ours.
  local function declarative_head(node, depth)
    if depth > MAX_DEPTH then
      return nil
    end
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "block_head" or kind == "generate_head" then
        return child
      elseif HEAD_WRAPPER[kind] then
        local found = declarative_head(child, depth + 1)
        if found then
          return found
        end
      end
    end
    return nil
  end

  -- A construct's contents stay under it: flattening a generate's instances
  -- into the enclosing unit hides which generate they belong to. Its head's
  -- declarations attach here, and its body's statements nest below.
  local function with_concurrent(entry, node, source, depth)
    depth = depth or 1
    local head = declarative_head(node, depth)
    if head then
      attach_lines(entry, declaration_entries(head, source))
    end
    if depth > MAX_DEPTH then
      return { entry }
    end
    local nested = {}
    for _, child in ipairs(node:named_children()) do
      -- `else` is its own entry, emitted beside this one; walking it here would
      -- repeat its contents under both.
      if child:type() == "if_generate" then
        for _, inner in ipairs(child:named_children()) do
          if inner:type() ~= "else_generate" then
            add_all(nested, concurrent_entries(inner, source, depth + 1))
          end
        end
      elseif child:type() ~= "else_generate" then
        add_all(nested, concurrent_entries(child, source, depth + 1))
      end
    end
    attach_lines(entry, nested)
    return { entry }
  end

  local function extract_block(node, source, depth)
    local label = label_of(node, source)
    local entry = new_entry(SECTION.Block, node, truncate(labelled(label, "block"), SIGNATURE_TRUNCATE))
    return with_concurrent(entry, node, source, depth)
  end

  -- An `if generate` and its `else` are two configurations of one statement, so
  -- they stand side by side rather than one inside the other.
  local function extract_generate(node, source, depth)
    local entry = new_entry(SECTION.Block, node, truncate(generate_label(node, source), SIGNATURE_TRUNCATE))
    local entries = with_concurrent(entry, node, source, depth)
    local label = label_of(node, source)
    for _, child in ipairs(node:named_children()) do
      for _, sub in ipairs(child:named_children()) do
        if sub:type() == "else_generate" then
          local text = label and (label .. " : else generate") or "else generate"
          local branch = new_entry(SECTION.Block, sub, truncate(text, SIGNATURE_TRUNCATE))
          add_all(entries, with_concurrent(branch, sub, source, depth))
        end
      end
    end
    return entries
  end

  local function extract_case_alternative(node, source, depth)
    local entry = new_entry(SECTION.Block, node, truncate(alternative_text(node, source), SIGNATURE_TRUNCATE))
    return with_concurrent(entry, node, source, depth)
  end

  -- A package's constants, types and subprograms are scoped to it, so they nest
  -- under it the way an architecture's declarations do. A declaration and a body
  -- are separate library units and name themselves separately, so a file holding
  -- both shows where each one starts.
  local function add_package_members(entry, node, source)
    local members = {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "package_definition_body" or kind == "package_declaration_body" then
        -- Subprograms share the package budget with the constants and types: a
        -- package exports all of them alike, and `attach_lines` already gives
        -- the function section its own share.
        add_all(members, declaration_entries(child, source))
      end
    end
    attach_lines(entry, members, PACKAGE_MEMBER_CAP)
    return entry
  end

  local function extract_package(node, source)
    local name = field_text(node, "package", source)
    if not name then
      return nil
    end
    local keyword = node:type() == "package_definition" and "package body " or "package "
    -- A design unit, not `mod:`: `mod:` joins entries into a bare name list and
    -- drops children, and the package's constants and types are its content.
    return add_package_members(new_entry(SECTION.DesignUnit, node, keyword .. name), node, source)
  end

  extract_nodes = function(node, source, _attrs, depth)
    local kind = node:type()
    if DECLARATIVE_PART[kind] then
      -- Its owner attaches these declarations.
      return {}
    elseif kind == "design_unit" then
      local entries = {}
      for _, child in ipairs(node:named_children()) do
        add_all(entries, extract_nodes(child, source))
      end
      return entries
    elseif kind == "context_reference" then
      local entry = simple_import(node, source, CONTEXT_PREFIXES, ".")
      return entry and { entry } or {}
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
      local entry = extract_package(node, source)
      return entry and { entry } or {}
    elseif kind == "subprogram_definition" or kind == "subprogram_declaration" then
      local entry = subprogram_entry(node, source)
      return entry and { entry } or {}
    elseif kind == "process_statement" then
      return process_entries(node, source)
    elseif kind == "block_statement" then
      return extract_block(node, source, depth)
    elseif GENERATE_KEYWORD[kind] then
      return extract_generate(node, source, depth)
    elseif kind == "case_generate_alternative" then
      return extract_case_alternative(node, source, depth)
    elseif kind == "package_header" then
      -- A generic package declares its interface types and subprograms here.
      -- The formals are package members, so `attach_lines` holds the budget for
      -- them; capping again per clause would cut them twice.
      local entries = {}
      for _, clause in ipairs(node:named_children()) do
        if clause:type() == "generic_clause" then
          for _, list in ipairs(clause:named_children()) do
            add_all(entries, declaration_entries(list, source, true))
          end
        end
      end
      return entries
    elseif CONCURRENT_CONTAINER[kind] then
      return concurrent_entries(node, source, depth)
    elseif kind == "component_instantiation_statement" or kind == "concurrent_procedure_call_statement" then
      return instantiation_entry(node, source)
    elseif kind == "configuration_declaration" then
      local entry = extract_configuration(node, source)
      return entry and { entry } or {}
    elseif kind == "package_instantiation_declaration" then
      local name = field_text(node, "package", source)
      local unit = field_text(node, "instantiated_unit", source)
      if not unit then
        for _, child in ipairs(node:named_children()) do
          if child:type() == "name" then
            unit = one_line(child, source)
          end
        end
      end
      if not name then
        return {}
      end
      -- A generic package instance is a package: same section, and the unit it
      -- instantiates is the thing a reader wants named.
      return {
        new_entry(
          SECTION.DesignUnit,
          node,
          truncate("package " .. name .. (unit and (" is new " .. unit) or ""), MEMBER_TRUNCATE)
        ),
      }
    elseif kind == "context_declaration" then
      return context_entries(node, source)
    end
    return {}
  end

  return {
    import_separator = ".",

    extract_nodes = extract_nodes,
  }
end
