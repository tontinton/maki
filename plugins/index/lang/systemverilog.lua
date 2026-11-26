return function(U)
  local get_text = U.get_text
  local compact_ws = U.compact_ws
  local one_line = U.one_line
  local statement_line = U.statement_line
  local field_text = U.field_text
  local add_all = U.add_all
  local truncate = U.truncate
  local new_entry = U.new_entry
  local new_import_entry = U.new_import_entry
  local ranged = U.ranged
  local format_range = U.format_range
  local SECTION = U.SECTION

  local PARAM_CAP = 16
  local PARAM_TRUNCATE = 80
  -- A package exports its members, so a hand-written one must list whole; the
  -- budget holds per container and per section, which keeps a generated file's
  -- register map bounded without hiding a small package's contents.
  local PACKAGE_MEMBER_CAP = 32
  local PORT_TRUNCATE = 64
  local ATTRIBUTE_CAP = 8
  local MEMBER_TRUNCATE = 60
  -- A container holds several kinds, so a cut says which kind it cut. The
  -- names match the sections a reader already sees.
  local MEMBER_KIND = {
    A = "attrs",
    M = "fns",
    T = "types",
    F = "nets",
    P = "procs",
    S = "ports",
    all = "consts",
  }
  local function kind_marker(cap, name)
    return function(key, total)
      return "[" .. (total - cap) .. " " .. (name or MEMBER_KIND[key] or "items") .. " truncated]"
    end
  end
  local SIGNATURE_TRUNCATE = 100
  -- A struct is usually a packet or a register map, so its field list is the
  -- content a reader came for rather than detail to trim.
  local RECORD_FIELD_CAP = 24

  local function has_typedef(node)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "type_declaration" then
        return true
      end
    end
    return false
  end

  -- Containers whose entry lists its own members through `with_members`, so a
  -- data declaration reached below them is already listed and must not also
  -- get an entry of its own.
  local MEMBER_CONTAINER = {
    module_declaration = true,
    interface_declaration = true,
    interface_class_declaration = true,
    class_declaration = true,
    package_declaration = true,
    program_declaration = true,
    checker_declaration = true,
  }

  -- True when the node sits inside a design unit, however many wrappers the
  -- header or a module item puts between them.
  local function inside_container(node)
    local up = node:parent()
    while up do
      if MEMBER_CONTAINER[up:type()] then
        return true
      end
      up = up:parent()
    end
    return false
  end

  -- Nodes whose children are indexed but which are not themselves an entry.
  -- Packages wrap each item in `package_item`, classes in `class_item` plus
  -- `class_property` or `class_method`; modules and interfaces hold theirs
  -- directly. `member_lines` walks only as far as `class_property`, because a
  -- method is not a field.
  local TRANSPARENT = {
    interface_item = true,
    interface_class_item = true,
    interface_class_method = true,
    package_item = true,
    class_item = true,
    class_property = true,
    class_method = true,
    checker_item = true,
    specify_block = true,
    specify_item = true,
  }

  -- Node kinds whose body block they own.
  local PROCEDURAL_CONSTRUCT = {
    initial_construct = true,
    always_construct = true,
    final_construct = true,
  }

  -- `1 ns / 10 ps` and `1ns/10ps` mean the same, so both spaces around the slash
  -- go before the number and unit are joined. A trailing `//` comment is not
  -- part of the value.
  local function timescale_value(node, source)
    local value = compact_ws(get_text(node, source)):gsub("//.*$", ""):gsub("^`", "")
    value = value:gsub("%s*/%s*", "/"):gsub("(%d)%s+([%a%%])", "%1%2")
    return (value:gsub("%s+$", ""))
  end

  -- Timing refinement is not shape, so the node carrying it is cut from the raw
  -- text by byte range rather than by pattern. Cutting source bytes rather than
  -- rebuilding from sibling nodes keeps a unit written apart from its number:
  -- that form is not legal but still parses, and the grammar then files the name
  -- under an error node, which is the only place the name survives.
  -- Cuts every child of `kind` out of the node's text. Each cut moves the text
  -- after it left, so the bytes dropped so far are subtracted from the next
  -- offset; without that, a node with two such children keeps a fragment.
  local function without_child(node, source, kind)
    local _, _, start, _, _, stop = node:range(true)
    local text = source:sub(start + 1, stop)
    local dropped = 0
    for _, child in ipairs(node:named_children()) do
      if not kind or child:type() == kind then
        local _, _, child_start, _, _, child_stop = child:range(true)
        text = text:sub(1, child_start - start - dropped) .. text:sub(child_stop - start - dropped + 1)
        dropped = dropped + (child_stop - child_start)
      end
    end
    return (compact_ws(text):gsub("%s+$", ""))
  end

  -- The grammar spells a type three ways (`data_type`, `data_type_or_void`,
  -- `data_type_or_implicit`) plus `variable_port_type` on ports; every call
  -- site that asks "is this the type?" means all four.
  local function is_type_node(kind)
    return kind:find("^data_type") ~= nil or kind == "variable_port_type"
  end

  -- Modules and interfaces name themselves on a `*_header` child, packages
  -- and classes on the declaration itself.
  local function declaration_name(node, source)
    local name = field_text(node, "name", source)
    if name then
      return name
    end
    for _, child in ipairs(node:named_children()) do
      if child:type():find("_header", 1, true) then
        return field_text(child, "name", source)
      elseif child:type() == "simple_identifier" then
        return get_text(child, source)
      end
    end
    return nil
  end

  -- The declared names hang off a hidden assignment list, so they are reached
  -- by walking for the assignment leaves. A type holds no declarators, so the
  -- walk skips it rather than descending.
  local function declared_names(node, source)
    local names = {}
    local stack = { node }
    while #stack > 0 do
      local current = table.remove(stack)
      if current:type() == "variable_decl_assignment" or current:type() == "param_assignment" then
        local name = field_text(current, "name", source)
          or current:named_child(0) and one_line(current:named_child(0), source)
        if name then
          names[#names + 1] = name
        end
      elseif not is_type_node(current:type()) then
        for _, child in ipairs(current:named_children()) do
          stack[#stack + 1] = child
        end
      end
    end
    return names
  end

  local function type_text(node, source)
    for _, child in ipairs(node:named_children()) do
      if is_type_node(child:type()) then
        return truncate(one_line(child, source), MEMBER_TRUNCATE)
      end
    end
    return nil
  end

  local function body_declaration(node, kind)
    for _, child in ipairs(node:named_children()) do
      if child:type() == kind then
        return child
      end
    end
    return nil
  end

  -- A function's return type is part of what tells two overloads apart, and a
  -- reader needs it to call the thing. A task returns nothing, and `void` says
  -- nothing, so both stay silent.
  local function return_type(node, source)
    if node:type() ~= "function_body_declaration" then
      return nil
    end
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "data_type_or_void" then
        local text = one_line(child, source)
        return text ~= "void" and text or nil
      end
    end
    return nil
  end

  local function signature_entry(node, source)
    local name = field_text(node, "name", source)
    if not name then
      return nil
    end
    local ports = body_declaration(node, "tf_port_list")
    local params = ports and truncate("(" .. one_line(ports, source) .. ")", SIGNATURE_TRUNCATE) or "()"
    local ret = return_type(node, source)
    return new_entry(SECTION.Function, node, (ret and (ret .. " ") or "") .. name .. params)
  end

  -- A field hides behind a property chain; a method hides behind
  -- `class_method` or a task/function wrapper and renders as its signature.
  local MEMBER_WRAPPER = {
    block_item_declaration = true,
    class_item = true,
    class_property = true,
    class_method = true,
    checker_item = true,
    interface_class_item = true,
    interface_class_method = true,
    interface_item = true,
    modport_declaration = true,
    module_item = true,
    package_item = true,
  }

  local PROTOTYPE = { function_prototype = true, task_prototype = true }

  -- A generate or procedural block names itself through a field, or through a
  -- leading identifier. One reader for both keeps every label spelled alike.
  local function block_name(node, source)
    local name = field_text(node, "name", source)
    if name then
      return name
    end
    local first = node:named_child(0)
    return first and first:type() == "simple_identifier" and get_text(first, source) or nil
  end

  -- `label : keyword`, so a labelled construct reads as one line.
  local function label_format(name, keyword)
    return name and (name .. " : " .. keyword) or keyword
  end

  -- A generate block carries no kind of its own; the construct that wraps it
  -- does. Spelling `g : for generate` the same way the VHDL parser does keeps
  -- the two indexes comparable.
  local GENERATE_KIND = {
    loop_generate_construct = "for",
    if_generate_construct = "if",
    case_generate_construct = "case",
    case_generate_item = "case",
  }

  -- A case generate item is identified only by its choice expression, so the
  -- choice joins the label; the body usually names itself as well.
  -- The `else` branch of an `if generate` shares its parent with the `if`
  -- branch, so the token that precedes it is what tells them apart.
  local function after_else(node)
    local previous = node:prev_sibling()
    return previous ~= nil and previous:type() == "else"
  end

  local function generate_keyword(node, source)
    local parent = node:parent()
    local kind = parent and GENERATE_KIND[parent:type()]
    if not kind then
      return "generate"
    end
    if after_else(node) then
      return "else generate"
    end
    if parent:type() ~= "case_generate_item" then
      return kind .. " generate"
    end
    for _, child in ipairs(parent:children()) do
      if child:type() == "default" then
        return kind .. " generate (default)"
      end
    end
    local choice = parent:named_child(0)
    return kind .. " generate (" .. (choice and one_line(choice, source) or "default") .. ")"
  end

  -- The event control names the clock and any asynchronous reset, which is the
  -- most consequential fact about the block. `always_comb` and `initial` take
  -- none, so absence is normal.
  local function event_control_text(node, source)
    local pending = { node }
    local head = 1
    while head <= #pending do
      local current = pending[head]
      head = head + 1
      for _, child in ipairs(current:named_children()) do
        local kind = child:type()
        if kind == "event_control" then
          return " " .. truncate(one_line(child, source), SIGNATURE_TRUNCATE)
        end
        if kind == "statement" or kind == "statement_item" or kind == "procedural_timing_control_statement" then
          pending[#pending + 1] = child
        end
      end
    end
    return ""
  end

  -- A bare `begin : n` inside a generate region is legal Verilog-2001 that the
  -- grammar reads as an `always_construct` whose `always` keyword is missing.
  -- The keyword spans no characters there, which is what tells the two apart.
  local function owned_by_procedural(node)
    local up = node:parent()
    while up and not PROCEDURAL_CONSTRUCT[up:type()] and not MEMBER_CONTAINER[up:type()] do
      up = up:parent()
    end
    if not up or not PROCEDURAL_CONSTRUCT[up:type()] then
      return false
    end
    local keyword = up:type() == "always_construct" and body_declaration(up, "always_keyword")
    if not keyword then
      return true
    end
    local sl, sc, el, ec = keyword:range()
    return sl ~= el or sc ~= ec
  end

  -- `label : keyword`, so two blocks in one module stay apart.
  local function procedural_text(node, source, owned_name)
    local kind = node:type()
    local keyword
    if kind == "always_construct" then
      local always = node:named_child(0)
      keyword = (always and get_text(always, source) or "always") .. event_control_text(node, source)
    else
      keyword = kind:gsub("_construct$", "")
    end
    -- Only the construct's own label names it. An unlabelled block stays
    -- unlabelled even inside a generate: that label belongs to the generate,
    -- which already carries it, and borrowing it would replace the block's name.
    return label_format(owned_name, keyword)
  end

  -- Wrappers a case statement can sit inside. A function body is reached by its
  -- own entry rather than from here.
  local PROCEDURAL_BODY = {
    seq_block = true,
    statement = true,
    statement_item = true,
    statement_or_null = true,
    procedural_timing_control_statement = true,
    function_statement = true,
    function_statement_or_null = true,
    conditional_statement = true,
    loop_statement = true,
    case_statement = true,
    case_item = true,
  }

  local extract_typedef

  local block_declarations

  local case_headers

  -- Synthesis and CDC intent an agent has to know about; everything else is
  -- vendor packaging metadata (`X_INTERFACE_*`, `XIL_*`), which says nothing
  -- about the design. Same rule Rust applies by keeping only `derive` and `cfg`.
  local ATTR_KEEP = {
    async_reg = true,
    dont_touch = true,
    full_case = true,
    keep = true,
    mark_debug = true,
    parallel_case = true,
    ram_style = true,
    rom_style = true,
    rtl_keep = true,
    translate_off = true,
    translate_on = true,
  }

  -- `(* keep = "true" *)`, delimiters included: nothing else in the skeleton
  -- looks like that, and without them the line reads as a declaration.
  local function attribute_text(node, source)
    local specs = {}
    for _, spec in ipairs(node:named_children()) do
      if spec:type() == "attr_spec" then
        local name = spec:named_child(0)
        if name and ATTR_KEEP[get_text(name, source):lower()] then
          specs[#specs + 1] = statement_line(spec, source)
        end
      end
    end
    if #specs == 0 then
      return nil
    end
    return truncate("(* " .. table.concat(specs, ", ") .. " *)", MEMBER_TRUNCATE)
  end

  local function member_lines(node, source, buckets, ports, attrs, pending_attrs)
    buckets = buckets or U.capped_buckets()
    attrs = attrs or U.capped_buckets(ATTRIBUTE_CAP)
    -- An attribute and its declaration can sit on either side of the one
    -- wrapper between them (`class_item` holds both, `class_property` holds the
    -- declaration), so the buffer follows the recursion. It is cleared in place
    -- because the caller keeps reading it.
    pending_attrs = pending_attrs or {}
    -- An attribute annotates the declarations around it rather than being a
    -- member, so it drains into its own budget; a generated file that marks
    -- every signal cannot push out the body's contents.
    local function flush_attrs()
      for _, attr in ipairs(pending_attrs) do
        attrs.push("A", attr)
      end
      for i = #pending_attrs, 1, -1 do
        pending_attrs[i] = nil
      end
    end
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "attribute_instance" then
        local text = attribute_text(child, source)
        if text then
          pending_attrs[#pending_attrs + 1] = ranged(text, format_range(U.line_start(child), U.line_end(child)))
        end
      else
        -- An attribute belongs to the next declaration whatever it is, so the
        -- buffer drains before any branch renders, rather than only on the few
        -- that remember to. A trailing one is drained after the loop.
        flush_attrs()
      end
      if MEMBER_WRAPPER[kind] then
        member_lines(child, source, buckets, ports, attrs, pending_attrs)
      elseif kind == "function_declaration" or kind == "task_declaration" then
        local body_kind = kind == "function_declaration" and "function_body_declaration" or "task_body_declaration"
        local body = body_declaration(child, body_kind)
        local entry = body and signature_entry(body, source)
        if entry then
          if body then
            add_all(entry.children, case_headers(body, source))
          end
          buckets.push("M", entry)
        end
      elseif kind == "net_declaration" then
        local text = truncate(without_child(child, source, "delay3"):gsub(";%s*$", ""), MEMBER_TRUNCATE)
        buckets.push("F", ranged(text, format_range(U.line_start(child), U.line_end(child))))
      elseif kind == "type_declaration" or (kind == "data_declaration" and has_typedef(child)) then
        -- A typedef inside a module is a type, not a field, so it lands in the
        -- types bucket with its members rather than being flattened into a
        -- single declaration line. An interface class holds its typedef bare,
        -- where a plain class wraps the same construct in a `data_declaration`.
        local entry = extract_typedef(child, source)
        if entry then
          buckets.push("T", entry)
        end
      elseif kind == "data_declaration" or kind == "struct_union_member" then
        -- Each assignment carries its own initializer, so the line is built
        -- from the assignment nodes rather than the names alone: dropping the
        -- expression would lose what the variable starts as.
        local parts = {}
        for _, sub in ipairs(child:named_children()) do
          local kind_sub = sub:type()
          if kind_sub == "list_of_variable_decl_assignments" then
            for _, assignment in ipairs(sub:named_children()) do
              if assignment:type() == "variable_decl_assignment" then
                parts[#parts + 1] = one_line(assignment, source)
              end
            end
          end
        end
        if #parts > 0 then
          local decl_type = type_text(child, source)
          buckets.push(
            "F",
            ranged(
              truncate((decl_type and (decl_type .. " ") or "") .. table.concat(parts, ", "), MEMBER_TRUNCATE),
              format_range(U.line_start(child), U.line_end(child))
            )
          )
        end
      elseif PROCEDURAL_CONSTRUCT[kind] then
        local decls, name = block_declarations(child, source)
        local entry =
          new_entry(SECTION.Block, child, truncate(procedural_text(child, source, name), SIGNATURE_TRUNCATE))
        add_all(entry.children, decls)
        add_all(entry.children, case_headers(child, source))
        buckets.push("P", entry)
      elseif PROTOTYPE[kind] or kind == "class_constructor_prototype" then
        -- The only signature an out-of-body method has, so it is the class's
        -- interface rather than a duplicate of its body.
        buckets.push(
          "M",
          ranged(
            truncate(statement_line(child, source), SIGNATURE_TRUNCATE),
            format_range(U.line_start(child), U.line_end(child))
          )
        )
      elseif kind == "clocking_declaration" then
        -- A clocking block declares the drive/sample timing a testbench works
        -- against: the event says when, the items say which signals are
        -- sampled or driven and with what skew.
        -- A clocking block takes a name, never a label, so the identifier is
        -- part of the declaration rather than a prefix.
        local name = block_name(child, source)
        local event = body_declaration(child, "clocking_event")
        local text = "clocking " .. (name and (name .. " ") or "") .. (event and one_line(event, source) or "")
        local entry = new_entry(SECTION.Type, child, truncate(text, PARAM_TRUNCATE))
        for _, item in ipairs(child:named_children()) do
          if item:type() == "clocking_item" then
            -- Direction and signal are what triage needs; skew is timing
            -- refinement, and a `default` item names no signal at all.
            local direction, signals
            for _, part in ipairs(item:named_children()) do
              if part:type() == "clocking_direction" then
                direction = without_child(part, source)
              elseif part:type() == "list_of_clocking_decl_assign" then
                signals = one_line(part, source)
              end
            end
            if direction and signals then
              entry.children[#entry.children + 1] = ranged(
                truncate(direction .. " " .. signals, MEMBER_TRUNCATE),
                format_range(U.line_start(item), U.line_end(item))
              )
            end
          end
        end
        buckets.push("T", entry)
      elseif kind == "modport_item" then
        buckets.push(
          "T",
          ranged(
            truncate("modport " .. one_line(child, source), PARAM_TRUNCATE),
            format_range(U.line_start(child), U.line_end(child))
          )
        )
      elseif kind == "port_declaration" then
        -- A port declared on a body statement (the non-ANSI style) is the only
        -- place its full shape appears. It is the interface, not body state, so
        -- it shares the port budget rather than the eight-member one. The
        -- grammar nests its `attribute_instance` children here, and their raw
        -- text would carry an attribute past the allowlist that filters it.
        local line = ranged(
          truncate(without_child(child, source, "attribute_instance"), MEMBER_TRUNCATE),
          format_range(U.line_start(child), U.line_end(child))
        )
        if ports then
          ports[#ports + 1] = line
        else
          buckets.push("S", line)
        end
      end
    end
    flush_attrs()
    return buckets, attrs
  end

  -- A module or interface names its ports in a `*_header` child, which the
  -- member walk does not descend into.
  local function port_lines(node, source)
    local lines = {}
    for _, header in ipairs(node:named_children()) do
      if header:type():find("_header", 1, true) then
        for _, list in ipairs(header:named_children()) do
          if list:type() == "list_of_port_declarations" then
            for _, port in ipairs(list:named_children()) do
              if port:type() == "ansi_port_declaration" then
                lines[#lines + 1] = ranged(
                  truncate(one_line(port, source), MEMBER_TRUNCATE),
                  format_range(U.line_start(port), U.line_end(port))
                )
              end
            end
          end
        end
      end
    end
    return lines
  end

  -- Parameters belong to the unit that declares them, so they nest under it. A
  -- header list and a body declaration say the same thing in two syntaxes.
  local function parameter_lines(node, source, member_cap)
    local cap = member_cap or PARAM_CAP
    local lines = U.capped_buckets(cap)
    local function add(node)
      lines.push(
        "all",
        ranged(
          truncate(statement_line(node, source), PARAM_TRUNCATE),
          format_range(U.line_start(node), U.line_end(node))
        )
      )
    end
    local function collect(item)
      for _, child in ipairs(item:named_children()) do
        local kind = child:type()
        if kind == "parameter_port_list" or kind == "parameter_port_declaration" then
          collect(child)
        elseif kind == "parameter_declaration" or kind == "local_parameter_declaration" then
          add(child)
        end
      end
    end
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "timeunits_declaration" then
        -- Module-scoped, unlike the file-scope `timescale`, so it nests here.
        -- The two literals are the unit and the precision, however the statement
        -- spells them, so the value is read from them rather than the text.
        local literals = {}
        for _, literal in ipairs(child:named_children()) do
          literals[#literals + 1] = compact_ws(get_text(literal, source)):gsub("(%d)%s+([%a%%])", "%1%2")
        end
        lines.push(
          "all",
          ranged("timeunit " .. table.concat(literals, "/"), format_range(U.line_start(child), U.line_end(child)))
        )
      elseif kind == "parameter_declaration" or kind == "local_parameter_declaration" then
        add(child)
      else
        collect(child)
      end
    end
    return lines.get("all", kind_marker(cap))
  end

  local extract_class

  -- The block a procedural construct holds: its declarations and its label.
  block_declarations = function(node, source)
    local decls = {}
    local name
    local pending = { node }
    local head = 1
    while head <= #pending do
      local current = pending[head]
      head = head + 1
      for _, child in ipairs(current:named_children()) do
        local kind = child:type()
        if kind == "block_item_declaration" then
          add_all(decls, parameter_lines(child, source))
          local lines, attrs_of = member_lines(child, source)
          add_all(decls, attrs_of.get("A"))
          add_all(decls, lines.get("F"))
        elseif PROCEDURAL_BODY[kind] then
          -- The construct's own `begin : label` sits below the event control,
          -- so the walk has to reach through the same wrappers a case does.
          if kind == "seq_block" then
            name = name or block_name(child, source)
          end
          pending[#pending + 1] = child
        end
      end
    end
    return decls, name
  end

  -- The grammar puts `case` in a `case_keyword` sibling and the selector in a
  -- `case_expression` that excludes the parentheses, so the parens are not part
  -- of the text; the keyword is restored from the grammar's own spelling.
  local function case_text(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "case_expression" then
        return truncate("case " .. one_line(child, source), SIGNATURE_TRUNCATE)
      end
    end
    return "case"
  end

  -- A case statement is raised because it is where a state machine or a decoder
  -- lives, and those two shapes are the bulk of what a clocked or combinatorial
  -- block does. Only the header is listed: a branch is a name, while its meaning
  -- is the assignment it holds, which never fits on a line and is what a reader
  -- still has to fetch. `if` is left alone because it carries the clock edge,
  -- resets and enables in almost every always block, so an entry per `if` would
  -- repeat the same line everywhere without naming anything.
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

  -- Ports, methods and signals each get their own budget, so a wide interface
  -- cannot starve the behaviour and a busy body cannot hide the interface. A
  -- hand-written module is far under the port ceiling and a generated IP
  -- wrapper is far over it, so the interface is listed whole without letting one
  -- wrapper's port list dominate the file.
  -- A class sits one wrapper down in a package and directly in a module, so
  -- both levels are searched rather than assuming the wrapper.
  local function class_declarations(node)
    if node:type() == "class_declaration" then
      return { node }
    end
    local found = {}
    for _, child in ipairs(node:named_children()) do
      if child:type() == "class_declaration" then
        found[#found + 1] = child
      end
    end
    return found
  end

  -- An entry knows its line and a rendered line carries it in its range string;
  -- a truncation marker has neither, so it sorts after its members.
  local function source_line(item)
    return (type(item) == "table" and (item.line_start or tonumber((item.range or ""):match("%[(%d+)")))) or math.huge
  end

  -- Concatenates the capped lists and puts them back in the order the file
  -- declares them. A marker has no line, so it sorts after its members.
  local function merge_members(lists)
    local merged = {}
    for _, list in ipairs(lists) do
      add_all(merged, list)
    end
    table.sort(merged, function(a, b)
      return source_line(a) < source_line(b)
    end)
    return merged
  end

  -- `dispatch` and `extract_nodes` call each other, so one is bound first.
  local extract_nodes

  local function with_members(entry, node, source, member_cap)
    local body_ports = {}
    local attrs = U.capped_buckets(ATTRIBUTE_CAP)
    local buckets = member_lines(node, source, U.capped_buckets(member_cap), body_ports, attrs)
    local ports = U.capped_buckets(PORT_TRUNCATE)
    for _, line in ipairs(port_lines(node, source)) do
      ports.push("S", line)
    end
    for _, line in ipairs(body_ports) do
      ports.push("S", line)
    end
    -- The buckets hold the budget per category; merging them back by line keeps
    -- the members in the order the file declares them. A header port and a
    -- body declaration sort together, because both carry their own line.
    local marker = kind_marker(member_cap or U.FIELD_TRUNCATE_THRESHOLD)
    local lists = {
      attrs.get("A", marker),
      parameter_lines(node, source, member_cap),
      ports.get("S", kind_marker(PORT_TRUNCATE)),
    }
    for _, key in ipairs({ "S", "M", "T", "F", "P" }) do
      lists[#lists + 1] = buckets.get(key, marker)
    end
    -- A class is an entry with members of its own, so it nests rather than
    -- being flattened into the container's line list. A package wraps one in
    -- `package_item`, where a module declares it directly. It joins the merge
    -- so it keeps its place among the members.
    for _, child in ipairs(node:named_children()) do
      for _, decl in ipairs(class_declarations(child)) do
        local nested = extract_class(decl, source)
        if nested then
          lists[#lists + 1] = { nested }
        end
      end
    end
    -- A program, an interface or a bare `generate ... endgenerate` is a
    -- container of its own, so it nests rather than being listed as a line.
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "program_declaration" or kind == "interface_declaration" or kind == "generate_region" then
        for _, nested in ipairs(extract_nodes(child, source)) do
          lists[#lists + 1] = { nested }
        end
      end
    end
    add_all(entry.children, merge_members(lists))
    return entry
  end

  local function extract_imports(node, source)
    local entries = {}
    for _, item in ipairs(node:named_children()) do
      if item:type() == "package_import_item" then
        local idents = {}
        for _, ident in ipairs(item:named_children()) do
          idents[#idents + 1] = one_line(ident, source)
        end
        local entry = #idents > 0 and new_import_entry(item, { idents })
        if entry then
          entries[#entries + 1] = entry
        end
      end
    end
    return entries
  end

  -- `automatic`/`static` parse as a `lifetime` sibling, so the body is not
  -- necessarily the first named child.
  local function extract_subprogram(node, source, body_kind)
    local body = body_declaration(node, body_kind)
    return body and signature_entry(body, source) or nil
  end

  local extract_function = function(node, source)
    return extract_subprogram(node, source, "function_body_declaration")
  end

  local extract_task = function(node, source)
    return extract_subprogram(node, source, "task_body_declaration")
  end

  -- `enum logic [1:0] { IDLE, RUN }`; the literals are listed separately, so
  -- only the head before the brace is kept here.
  local function enum_head(data_type, source)
    local text = one_line(data_type, source)
    return (text:gsub("%s*{.*$", ""))
  end

  local function enum_literals(data_type, source)
    local literals = {}
    for _, child in ipairs(data_type:named_children()) do
      if child:type() == "enum_name_declaration" then
        literals[#literals + 1] = one_line(child, source)
      end
    end
    return literals
  end

  -- An enum body lists its literals, a struct or union body its members. Both
  -- sit as siblings of the shape marker under the `data_type`, so the walk
  -- starts there in either case.
  local function typedef_body(data_type, source)
    for _, child in ipairs(data_type:named_children()) do
      local kind = child:type()
      if kind == "enum_name_declaration" then
        return enum_literals(data_type, source)
      elseif kind == "struct_union" then
        -- A struct or union holds only fields, so the buckets flatten to one.
        local bucket = U.capped_buckets(RECORD_FIELD_CAP)
        return member_lines(data_type, source, bucket).get("F", kind_marker(RECORD_FIELD_CAP, "fields"))
      end
    end
    return {}
  end

  -- `typedef <data_type> <name>` names the type, and a class member wraps the
  -- whole thing in a `type_declaration`, so the name and the type are read from
  -- whichever node carries them rather than from one fixed shape.
  extract_typedef = function(node, source)
    local declaration = node
    if node:type() == "data_declaration" then
      for _, child in ipairs(node:named_children()) do
        if child:type() == "type_declaration" then
          declaration = child
          break
        end
      end
    end
    local name = field_text(declaration, "type_name", source)
    if not name then
      return nil
    end
    local data_type = nil
    for _, child in ipairs(declaration:named_children()) do
      if child:type() == "data_type" then
        data_type = child
        break
      end
    end
    local text = "typedef " .. name .. (data_type and (" = " .. enum_head(data_type, source)) or "")
    local entry = new_entry(SECTION.Type, node, truncate(text, PARAM_TRUNCATE))
    if not data_type then
      return entry
    end
    entry.children = typedef_body(data_type, source)
    return entry
  end

  -- `class B extends A` carries the parent in a `class_type` child; interfaces
  -- and packages have nothing comparable.
  local function extends_suffix(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "class_type" then
        return " extends " .. one_line(child, source)
      end
    end
    return ""
  end

  extract_class = function(node, source, keyword)
    local name = declaration_name(node, source)
    if not name then
      return nil
    end
    local label = (keyword or "class ") .. name .. extends_suffix(node, source)
    return with_members(new_entry(SECTION.Class, node, label), node, source)
  end

  -- Modports name the directions a bus port is viewed through, which is the
  -- interface's real contract; the members alone do not show them.
  local function extract_modport(node, source)
    local entries = {}
    for _, item in ipairs(node:named_children()) do
      if item:type() == "modport_item" then
        local name = item:named_child(0)
        if name then
          entries[#entries + 1] =
            new_entry(SECTION.Type, item, truncate("modport " .. one_line(item, source), PARAM_TRUNCATE))
        end
      end
    end
    return entries
  end

  -- A property, sequence or covergroup names itself on a leading identifier.
  local function named_entry(node, source, keyword, section)
    for _, child in ipairs(node:named_children()) do
      if child:type() == "simple_identifier" then
        return new_entry(section, node, truncate(keyword .. " " .. get_text(child, source), MEMBER_TRUNCATE))
      end
    end
    return nil
  end

  -- A gate primitive (`and g1 (q, a, b);`) names its type through a gatetype
  -- node rather than a field, and holds its instances the same way a module
  -- does, so both read alike once the type and the instance children are found.
  local GATE = {
    gate_instantiation = true,
  }

  local function gate_type(node, source)
    for _, child in ipairs(node:named_children()) do
      if child:type():find("gatetype", 1, true) then
        local _, _, start, _, _, stop = child:range(true)
        return compact_ws(source:sub(start + 1, stop))
      end
    end
    return nil
  end

  -- `sub u1 (...)`: the instantiated type names the entry, the instance name
  -- the label.
  local function extract_instances(node, source)
    local is_gate = GATE[node:type()] == true
    local unit = is_gate and gate_type(node, source) or field_text(node, "instance_type", source)
    if not unit then
      return {}
    end
    local entries = {}
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      if kind == "hierarchical_instance" or (is_gate and kind:find("gate_instance", 1, true)) then
        local name_node = child:named_child(0)
        local name = name_node and field_text(name_node, "instance_name", source)
        entries[#entries + 1] = new_entry(
          SECTION.Impl,
          child,
          truncate(name and (name .. " : instance " .. unit) or ("instance " .. unit), MEMBER_TRUNCATE)
        )
      end
    end
    return entries
  end

  -- Containers that render their members as nested children. The keyword is
  -- what a reader sees, so it is the only part that differs between them.
  local DESIGN_UNIT = {
    module_declaration = "module",
    program_declaration = "program",
    interface_declaration = "interface",
    package_declaration = "package",
  }

  local function dispatch(node, source)
    local kind = node:type()
    if kind == "function_declaration" then
      local entry = extract_function(node, source)
      return entry and { entry } or {}
    elseif kind == "task_declaration" then
      local entry = extract_task(node, source)
      return entry and { entry } or {}
    elseif PROTOTYPE[kind] then
      local entry = signature_entry(node, source)
      return entry and { entry } or {}
    elseif kind == "modport_declaration" then
      return extract_modport(node, source)
    elseif kind == "module_instantiation" or GATE[kind] then
      return extract_instances(node, source)
    elseif kind == "property_declaration" or kind == "sequence_declaration" then
      local entry = named_entry(node, source, kind:gsub("_declaration$", ""), SECTION.Rule)
      return entry and { entry } or {}
    elseif kind == "covergroup_declaration" then
      local entry = named_entry(node, source, "covergroup", SECTION.Block)
      return entry and { entry } or {}
    elseif kind == "concurrent_assertion_item" then
      -- `assert property (p)` only names a property declared elsewhere, so the
      -- useful text is already in `rules:` from the declaration.
      return {}
    elseif kind == "generate_block" then
      -- A generate holds structure, so its contents are entries rather than
      -- rendered lines. `extract_nodes` already turns a body declaration into a
      -- titled entry, so the member-line pass would list it a second time. An
      -- unlabelled block still names its kind, so a reader can tell a
      -- conditional instance from an unconditional one.
      local entry =
        new_entry(SECTION.Block, node, label_format(block_name(node, source), generate_keyword(node, source)))
      local lines, attrs_of = member_lines(node, source)
      local extracted = {}
      local marker = kind_marker(U.FIELD_TRUNCATE_THRESHOLD)
      for _, child in ipairs(node:named_children()) do
        -- A declaration is rendered by the member walk, which handles a net and
        -- a variable alike and filters attributes; an instance or a nested
        -- generate is an entry of its own, and joins the merge so it keeps its
        -- place among the declarations.
        local child_kind = child:type()
        if
          child_kind ~= "net_declaration"
          and child_kind ~= "data_declaration"
          and child_kind ~= "parameter_declaration"
        then
          add_all(extracted, extract_nodes(child, source))
        end
      end
      add_all(
        entry.children,
        merge_members({
          attrs_of.get("A", marker),
          parameter_lines(node, source),
          lines.get("F", marker),
          lines.get("P", marker),
          extracted,
        })
      )
      return { entry }
    elseif kind == "seq_block" then
      -- A block inside a procedural construct is already named by that
      -- construct, which lists the declarations inside it, so an entry here
      -- would only duplicate it. Anywhere else the block stands for itself.
      if owned_by_procedural(node) then
        return {}
      end
      local text = label_format(block_name(node, source), "block")
      return { with_members(new_entry(SECTION.Block, node, text), node, source) }
    end
    local name = declaration_name(node, source)
    if not name then
      return {}
    end
    -- `types:`, not `mod:`: `mod:` joins entries into a bare name list and drops
    -- children, which would lose the port list. Two packages in one file are
    -- legal, so nesting is the only encoding that keeps each one's members tied
    -- to it. A checker is `blocks:`, since it holds statements, not members.
    local keyword = DESIGN_UNIT[kind]
    if keyword then
      local cap = kind == "package_declaration" and PACKAGE_MEMBER_CAP or nil
      return { with_members(new_entry(SECTION.DesignUnit, node, keyword .. " " .. name), node, source, cap) }
    elseif kind == "checker_declaration" then
      return { with_members(new_entry(SECTION.Block, node, "checker " .. name), node, source) }
    elseif kind == "class_declaration" then
      local entry = extract_class(node, source)
      return entry and { entry } or {}
    elseif kind == "interface_class_declaration" then
      -- A class kind the grammar spells separately, so it needs its own arm:
      -- without one the declaration and its prototypes vanish entirely.
      local entry = extract_class(node, source, "interface class ")
      return entry and { entry } or {}
    end
    return {}
  end

  -- Wrappers whose container renders their members as children, rather than
  -- listing them as entries of their own.
  local CHILD_RENDERING_WRAPPER = {
    class_item = true,
    class_property = true,
    class_method = true,
    interface_class_item = true,
    checker_item = true,
  }

  -- Generate constructs nest their blocks a few wrappers down.
  local GENERATE_WRAPPER = {
    conditional_generate_construct = true,
    if_generate_construct = true,
    case_generate_construct = true,
    case_generate_item = true,
    loop_generate_construct = true,
    generate_region = true,
  }

  local function is_nested_import(node)
    if node:type() ~= "data_declaration" then
      return false
    end
    for _, child in ipairs(node:named_children()) do
      if child:type() == "package_import_declaration" then
        return true
      end
    end
    return false
  end

  local function listed_as_child(node)
    local kind = node:type()
    return kind == "function_declaration"
      or kind == "task_declaration"
      or kind == "modport_declaration"
      or kind == "class_constructor_prototype"
      or kind == "class_declaration"
      -- `with_members` renders these as nested entries.
      or kind == "program_declaration"
      or kind == "interface_declaration"
      or kind == "generate_region"
      or PROTOTYPE[kind]
      -- An import is a `data_declaration` the member walk never lists, so it has
      -- to keep its own entry or the package loses it.
      or (kind == "data_declaration" and not is_nested_import(node))
  end

  local function is_nested_typedef(node)
    return node:type() == "data_declaration" and has_typedef(node)
  end

  -- A container that hides the types declared inside it. HDL wraps every file in
  -- a package or module, so hiding everything nested would hide every type; the
  -- rule that carries over from other languages is depth, not nesting.
  local HIDES_TYPEDEFS = {
    class_declaration = true,
    class_item = true,
    interface_class_declaration = true,
    interface_class_item = true,
  }

  -- Walks a container's wrappers recursively rather than queueing them, so a
  -- nested generate's blocks land where they sit instead of after every
  -- sibling of the level above. Each wrapper carries the `hides_typedefs` of
  -- its container, so a class reached through `class_item`/`class_property`
  -- still hides the types below it.
  local function inner_entries(node, source, lists_children, inherited)
    local entries = {}
    local hides_typedefs = inherited or HIDES_TYPEDEFS[node:type()] == true
    for _, child in ipairs(node:named_children()) do
      local kind = child:type()
      -- The module and interface headers hold the parameter ports; a non-ANSI
      -- module wraps every body item in a `module_item`. The construct wrappers
      -- of a generate hold the blocks themselves.
      -- Skipped on purpose: the container renders this child, or a global entry
      -- would claim a scope the type does not have.
      local skipped = (lists_children and listed_as_child(child))
        or (lists_children and hides_typedefs and is_nested_typedef(child))
      if not skipped then
        if kind:find("_header", 1, true) or kind == "module_item" or GENERATE_WRAPPER[kind] or TRANSPARENT[kind] then
          add_all(entries, inner_entries(child, source, lists_children, hides_typedefs))
        else
          add_all(entries, extract_nodes(child, source))
        end
      end
    end
    return entries
  end

  extract_nodes = function(node, source, _attrs)
    local kind = node:type()
    if TRANSPARENT[kind] then
      -- A wrapper whose container already rendered it through `with_members` had
      -- its children listed, so an entry below it would double them.
      return inner_entries(node, source, CHILD_RENDERING_WRAPPER[kind] == true)
    elseif kind == "parameter_port_list" or kind == "parameter_port_declaration" then
      -- Collected as children by `with_members`, so an entry here would double it.
      return {}
    elseif
      kind == "generate_region"
      or kind == "loop_generate_construct"
      or kind == "conditional_generate_construct"
    then
      return inner_entries(node, source)
    elseif PROCEDURAL_CONSTRUCT[kind] or kind:find("^always") or kind:find("^statement") then
      return inner_entries(node, source)
    elseif MEMBER_CONTAINER[kind] then
      local entries = dispatch(node, source)
      -- Only a container whose entry lists members through `with_members` may
      -- drop its data declarations here; it would otherwise lose them.
      for _, entry in ipairs(inner_entries(node, source, true)) do
        entries[#entries + 1] = entry
      end
      return entries
    elseif kind == "package_import_declaration" then
      return extract_imports(node, source)
    elseif kind == "specparam_declaration" then
      -- A specify block holds a dozen delay constants in one statement, so
      -- listing them truncates into noise. Naming it with its range tells a
      -- reader the timing is specified without spending the budget on a fragment.
      return { new_entry(SECTION.Constant, node, "specparam") }
    elseif kind == "timescale_compiler_directive" then
      -- A wrong timescale makes a simulation misbehave; the value says which.
      -- Its node ends on the newline, which would report a two-line range.
      local entry = new_entry(SECTION.Constant, node, timescale_value(node, source))
      entry.line_end = entry.line_start
      return { entry }
    elseif kind == "parameter_declaration" or kind == "local_parameter_declaration" then
      -- A parameter inside a unit is collected by `with_members`; only a
      -- file-scope one gets an entry of its own.
      if inside_container(node) then
        return {}
      end
      -- The type and the value are what make a parameter worth naming, so the
      -- line keeps them: `parameter [17:0] LO = 74694`, not `parameter LO`.
      local text = statement_line(node, source)
      return { new_entry(SECTION.Constant, node, truncate(text, PARAM_TRUNCATE)) }
    elseif kind == "data_declaration" then
      for _, child in ipairs(node:named_children()) do
        local inner_kind = child:type()
        if inner_kind == "type_declaration" then
          local entry = extract_typedef(child, source)
          return entry and { entry } or {}
        elseif inner_kind == "package_import_declaration" then
          -- The grammar wraps an import in a `data_declaration`, so it would
          -- otherwise be read as a variable declaration.
          return extract_imports(child, source)
        end
      end
      local decl_type = type_text(node, source)
      local names = declared_names(node, source)
      if #names == 0 then
        return {}
      end
      return {
        new_entry(
          SECTION.Constant,
          node,
          truncate((decl_type and (decl_type .. " ") or "") .. table.concat(names, ", "), MEMBER_TRUNCATE)
        ),
      }
    end
    return dispatch(node, source)
  end

  return {
    import_separator = "::",

    is_attr = function(node)
      return node:type() == "attribute_instance"
    end,

    extract_nodes = extract_nodes,
  }
end
