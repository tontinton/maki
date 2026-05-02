-- BUILD / BUILD.bazel indexer.
--
-- BUILD files are target-heavy. Keep the declarations that help navigation:
-- loads, package_groups, exports_files, top-level bindings, and named rule
-- calls. package() is valid but usually too broad to be useful in the index.

return function(U)
  local A = require("lang.bazel.analyze")(U)
  local R = require("lang.bazel.render")(U)
  local util = require("lang.bazel.util")

  local function push_binding(stmt, state)
    if not stmt.name then
      return
    end

    stmt.value = U.truncate(stmt.value_text_compact, util.BINDING_TRUNCATE)
    state.bindings[#state.bindings + 1] = stmt
  end

  local function push_package_group(stmt, state)
    local name = stmt:kwarg("name")

    if not name then
      return
    end

    state.package_groups[#state.package_groups + 1] = {
      name = name.text,
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
  end

  local function push_exports_files(stmt, state)
    local files = stmt:positional_or_kwarg(1, "srcs")

    if not files then
      return
    end

    state.exports_files[#state.exports_files + 1] = {
      files = files.text,
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
  end

  local function push_target(stmt, state)
    local name = stmt:kwarg("name")

    if not name then
      return
    end

    state.targets[#state.targets + 1] = {
      name = name.text,
      rule = stmt.target,
      deprecated = stmt:kwarg_present("deprecation"),
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
  end

  local call_handlers = {
    package_group = push_package_group,
    exports_files = push_exports_files,
  }

  -- BUILD calls we skip from the index. Currently just package(); its
  -- visibility/default args are too broad to surface as navigable structure.
  local IGNORED_CALLS = {
    package = true,
  }

  local function handle_call(stmt, state)
    if IGNORED_CALLS[stmt.target] then
      return
    end

    local handler = call_handlers[stmt.target]

    if handler then
      handler(stmt, state)
      return
    end

    push_target(stmt, state)
  end

  local function render_loads(state)
    return R.render_items("loads", state.loads, function(load)
      local module = load.module_quoted and util.quote(load.module) or load.module
      return R.label_line(module, table.concat(load.names, ", "), R.item_range(load))
    end)
  end

  local function render_package_groups(state)
    return R.render_items("package_groups", state.package_groups, function(group)
      return R.label_line(group.name, nil, R.item_range(group), { indent = 2 })
    end)
  end

  local function render_exports_files(state)
    return R.render_items("exports_files", state.exports_files, function(export)
      return R.item_line(export.files, R.item_range(export), { indent = 2 })
    end)
  end

  local function render_bindings(state)
    return R.render_items("variable bindings", state.bindings, function(binding)
      return R.label_line(binding.name, binding.value, R.item_range(binding), { indent = 2 })
    end)
  end

  local function render_targets(state)
    return R.render_items("targets", state.targets, function(target)
      local deprecated = target.deprecated and ", deprecated=True" or ""

      return R.label_line(target.name, target.rule .. deprecated, R.item_range(target), { indent = 2 })
    end)
  end

  local function render(state, collected)
    local sections = {}

    R.append_section_if_present(sections, R.render_doc("build", collected))
    R.append_section_if_present(sections, render_loads(state))
    R.append_section_if_present(sections, render_package_groups(state))
    R.append_section_if_present(sections, render_exports_files(state))
    R.append_section_if_present(sections, render_bindings(state))
    R.append_section_if_present(sections, render_targets(state))

    return table.concat(sections, "\n")
  end

  return {
    extract = function(source, root)
      local state = {
        loads = {},
        package_groups = {},
        exports_files = {},
        bindings = {},
        targets = {},
      }
      local collected = A.collect(root, source)

      for _, stmt in ipairs(collected.statements) do
        if stmt.kind == "load" then
          state.loads[#state.loads + 1] = stmt
        elseif stmt.kind == "assignment" then
          push_binding(stmt, state)
        elseif stmt.kind == "call" and stmt.target then
          handle_call(stmt, state)
        end
      end

      return render(state, collected)
    end,
  }
end
