-- Statement classification for the Bazel indexer.
--
-- Top-level AST nodes are classified into records keyed by `kind`:
--   load        { module, module_quoted, names[], line_start, line_end }
--   assignment  { name, value_text_raw, value_text_compact,
--                 value_call?, line_start, line_end }
--   function    { name, params, line_start, line_end }
--   call        CallRecord (with metatable; methods cover kwarg/positional access)
--
-- Each call's arguments are walked once at classify time. Positional and
-- keyword args become "value records" stored on the record, so extractors
-- never have to revisit the tree-sitter AST.
--
-- Extractors require this module (and render.lua / util.lua); they do not
-- require ast.lua. That keeps tree-sitter primitives behind a single
-- boundary owned by analyze.lua.

return function(U)
  local ast = require("lang.bazel.ast")(U)

  local A = {}

  ----------------------------------------------------------------------
  -- value records & call args (shared by classifiers below)
  ----------------------------------------------------------------------

  -- A value record represents one argument's value. `text` is always
  -- populated so callers don't special-case node kinds:
  --   "foo"     -> { kind = "string",     text = "foo",       quoted = true }
  --   FOO       -> { kind = "identifier", text = "FOO",       quoted = false }
  --   any expr  -> { kind = "other",      text = "<compact>", quoted = false }
  local function new_value_record(node, source)
    local t = node:type()

    if t == "identifier" then
      return { kind = "identifier", text = ast.get_text(node, source), quoted = false }
    elseif t == "string" then
      local content = ast.raw_string_contents(node, source)
      if content then
        return { kind = "string", text = content, quoted = true }
      end
    elseif t == "list" then
      return { kind = "other", text = ast.compact_list(node, source), quoted = false }
    end

    -- Default: anything else (and malformed string literals that fail to
    -- parse) renders as compact source text.
    return { kind = "other", text = ast.compact_ws_node(node, source), quoted = false }
  end

  -- Walk a call's args once. Returns three views:
  --   positional[]  value records by 1-based positional index
  --   kwargs        table by name, value records
  --   args[]        all args in source order, each
  --                 { kind = "positional", value = vrec } or
  --                 { kind = "keyword", name = "...", value = vrec }
  local function extract_call_args(call, source)
    local positional = {}
    local kwargs = {}
    local args = {}

    for _, arg in ipairs(ast.call_args(call)) do
      local entry

      if arg:type() == "keyword_argument" then
        local name = ast.keyword_name(arg, source)
        local value_node = ast.keyword_value(arg)

        if name and value_node then
          entry = { kind = "keyword", name = name, value = new_value_record(value_node, source) }
          kwargs[name] = entry.value
        end

        -- Malformed keyword arg (no name or no value) is silently dropped.
      elseif arg:type() == "dictionary_splat" then
        entry = { kind = "dictionary_splat", keys = ast.dictionary_splat_keys(arg, source) }
      else
        entry = { kind = "positional", value = new_value_record(arg, source) }
        positional[#positional + 1] = entry.value
      end

      if entry then
        args[#args + 1] = entry
      end
    end

    return positional, kwargs, args
  end

  ----------------------------------------------------------------------
  -- CallRecord
  --
  -- Methods (kwarg, kwarg_string, kwarg_bool, ...) return value records or
  -- nil; the value record schema is documented above.
  ----------------------------------------------------------------------

  local CallRecord = {}
  CallRecord.__index = CallRecord

  function CallRecord:kwarg(name)
    return self.kwargs[name]
  end

  function CallRecord:kwarg_present(name)
    return self.kwargs[name] ~= nil
  end

  function CallRecord:kwarg_string(name)
    local v = self.kwargs[name]
    if v and v.kind == "string" then
      return v.text
    end
    return nil
  end

  function CallRecord:kwarg_bool(name)
    local v = self.kwargs[name]
    if not v then
      return nil
    end
    if v.text == "True" then
      return true
    end
    if v.text == "False" then
      return false
    end
    return nil
  end

  function CallRecord:positional_at(i)
    return self.positional[i]
  end

  function CallRecord:positional_or_kwarg(i, name)
    return self.positional[i] or self.kwargs[name]
  end

  function CallRecord.new(call, source)
    local positional, kwargs, args = extract_call_args(call, source)
    local s, e = ast.node_lines(call)

    return setmetatable({
      kind = "call",
      target = ast.call_target(call, source),
      positional = positional,
      kwargs = kwargs,
      args = args,
      line_start = s,
      line_end = e,
    }, CallRecord)
  end

  ----------------------------------------------------------------------
  -- per-kind classifiers
  ----------------------------------------------------------------------

  local function call_record(node, source)
    local call = ast.unwrap_to(node, "call")
    if not call then
      return nil
    end
    return CallRecord.new(call, source)
  end

  -- The first arg is the module path; subsequent args are imported names.
  -- `module_quoted` is true iff the module came from a string literal, so
  -- extractors can render accordingly: BUILD keeps both shapes, .bzl drops
  -- non-quoted modules. For a keyword-form import (`alias = "real"`) the
  -- displayed name is the alias.
  local function load_record(node, source)
    local call = ast.unwrap_to(node, "call")
    if not call then
      return nil
    end

    if ast.call_target(call, source) ~= "load" then
      return nil
    end

    local _, _, args = extract_call_args(call, source)
    if #args == 0 then
      return nil
    end

    local module_vrec = args[1].value
    local names = {}

    for i = 2, #args do
      local entry = args[i]
      if entry.kind == "keyword" then
        names[#names + 1] = entry.name
      else
        names[#names + 1] = entry.value.text
      end
    end

    local s, e = ast.node_lines(call)

    return {
      kind = "load",
      module = module_vrec.text,
      module_quoted = module_vrec.quoted,
      names = names,
      line_start = s,
      line_end = e,
    }
  end

  local function assignment_record(node, source)
    local assignment = ast.unwrap_to(node, "assignment")
    if not assignment then
      return nil
    end

    local value_node = ast.assignment_value(assignment)
    if not value_node then
      return nil
    end

    local s, e = ast.node_lines(assignment)

    -- Two text forms are precomputed because the extractors render bindings
    -- differently: .bzl keeps the source layout (multi-line dicts/lists stay
    -- as-written), BUILD collapses everything onto one line. module.lua uses
    -- neither form -- it asks `value_call` for structured access.
    local rec = {
      kind = "assignment",
      name = ast.assignment_name(assignment, source),
      value_text_raw = ast.get_text(value_node, source),
      value_text_compact = ast.compact_ws_node(value_node, source),
      line_start = s,
      line_end = e,
    }

    -- value_call is a full CallRecord (metatabled) so module.lua's
    -- handle_extension_assignment can ask :positional_or_kwarg / :kwarg_bool
    -- on the RHS call directly.
    if value_node:type() == "call" then
      rec.value_call = CallRecord.new(value_node, source)
    end

    return rec
  end

  local function function_record(node, source)
    -- function_definition is a top-level statement form, so it may appear
    -- directly (unwrapped) as well as inside an expression_statement.
    local inner = ast.unwrap_to(node, "function_definition")

    if not inner then
      return nil
    end

    local name_node = inner:field("name")[1]
    if not name_node then
      return nil
    end

    local params_node = inner:field("parameters")[1]
    local s, e = ast.node_lines(inner)

    return {
      kind = "function",
      name = ast.get_text(name_node, source),
      params = params_node and ast.compact_params(params_node, source) or "()",
      line_start = s,
      line_end = e,
    }
  end

  ----------------------------------------------------------------------
  -- top-level dispatch
  ----------------------------------------------------------------------

  -- load_record runs before call_record because load() is also a call.
  local classify_steps = {
    load_record,
    assignment_record,
    function_record,
    call_record,
  }

  function A.classify(node, source)
    for _, step in ipairs(classify_steps) do
      local stmt = step(node, source)
      if stmt then
        return stmt
      end
    end

    return nil
  end

  function A.collect(root, source)
    local doc, nodes = ast.split_preamble(root, source)
    local statements = {}

    for _, node in ipairs(nodes) do
      local stmt = A.classify(node, source)
      if stmt then
        statements[#statements + 1] = stmt
      end
    end

    return { doc = doc, statements = statements }
  end

  return A
end
