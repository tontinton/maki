-- .bzl file indexer.
--
-- .bzl files are closest to Python modules: module docs, loads, top-level
-- bindings, and function signatures. Binding values keep raw whitespace before
-- truncation so multi-line structures retain their shape.

return function(U)
  local A = require("lang.bazel.analyze")(U)
  local R = require("lang.bazel.render")(U)
  local util = require("lang.bazel.util")

  local function handle_assignment(stmt, state)
    if not stmt.name then
      return
    end

    stmt.value = U.truncate(stmt.value_text_raw, util.BINDING_TRUNCATE)
    state.bindings[#state.bindings + 1] = stmt
  end

  local function handle_function(stmt, state)
    state.functions[#state.functions + 1] = stmt
  end

  -- .bzl skips loads whose module is not a string literal. They are valid
  -- Starlark but rare and ambiguous to render, so dropping them keeps the
  -- index focused.
  local function handle_load(stmt, state)
    if not stmt.module_quoted then
      return
    end

    state.loads[#state.loads + 1] = stmt
  end

  local stmt_handlers = {
    load = handle_load,
    assignment = handle_assignment,
    ["function"] = handle_function,
  }

  local function render_loads(state)
    return R.render_items("loads", state.loads, function(load)
      return R.label_line(util.quote(load.module), table.concat(load.names, ", "), R.item_range(load))
    end)
  end

  local function render_bindings(state)
    return R.render_items("variable bindings", state.bindings, function(binding)
      return R.item_line(binding.name .. " = " .. binding.value, R.item_range(binding))
    end)
  end

  local function render_functions(state)
    return R.render_items("functions", state.functions, function(func)
      return R.item_line(func.name .. func.params, R.item_range(func))
    end)
  end

  local function render(state, collected)
    local sections = {}

    R.append_section_if_present(sections, R.render_doc("module", collected))
    R.append_section_if_present(sections, render_loads(state))
    R.append_section_if_present(sections, render_bindings(state))
    R.append_section_if_present(sections, render_functions(state))

    return table.concat(sections, "\n")
  end

  return {
    extract = function(source, root)
      local state = {
        loads = {},
        bindings = {},
        functions = {},
      }
      local collected = A.collect(root, source)

      for _, stmt in ipairs(collected.statements) do
        local handler = stmt_handlers[stmt.kind]
        if handler then
          handler(stmt, state)
        end
      end

      return render(state, collected)
    end,
  }
end
