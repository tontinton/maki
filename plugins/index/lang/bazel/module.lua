-- MODULE.bazel indexer.
--
-- Bzlmod uses aliases heavily. use_extension() and use_repo_rule()
-- assignments seed alias entries; later calls such as llvm.toolchain(...) and
-- http_file(...) are indexed only when their aliases were declared earlier.

return function(U)
  local A = require("lang.bazel.analyze")(U)
  local R = require("lang.bazel.render")(U)
  local util = require("lang.bazel.util")

  local function dev_dependency(call)
    return call:kwarg_bool("dev_dependency") or false
  end

  local function dev_text(item, text)
    return item.dev_dependency and text or ""
  end

  local function is_alias_of(state, name, kind)
    return state.aliases[name] == kind
  end

  local function push_repo(state, rule, names, line_start, line_end, dev_dependency)
    state.repos[#state.repos + 1] = {
      rule = rule,
      names = names,
      dev_dependency = dev_dependency or false,
      line_start = line_start,
      line_end = line_end,
    }
  end

  local function handle_extension_assignment(stmt, state)
    local call = stmt.value_call
    local path = call:positional_or_kwarg(1, "extension_bzl_file")
    local extension_name = call:positional_or_kwarg(2, "extension_name")

    if not path or not extension_name then
      return
    end

    state.aliases[stmt.name] = "extension"
    state.extensions[#state.extensions + 1] = {
      alias = stmt.name,
      path = util.render_value(path),
      name = util.render_value(extension_name),
      dev_dependency = dev_dependency(call),
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
  end

  local function has_repo_rule_args(stmt)
    local call = stmt.value_call

    return call:positional_or_kwarg(1, "repo_rule_bzl_file") ~= nil
      and call:positional_or_kwarg(2, "repo_rule_name") ~= nil
  end

  local function handle_assignment(stmt, state)
    if not stmt.name then
      return
    end

    state.aliases[stmt.name] = nil

    if stmt.value_call then
      local target = stmt.value_call.target

      if target == "use_extension" then
        handle_extension_assignment(stmt, state)
        return
      end

      if target == "use_repo_rule" then
        if has_repo_rule_args(stmt) then
          state.aliases[stmt.name] = "repo_rule"
        end
        return
      end
    end

    if util.is_constant_name(stmt.name) then
      state.vars[#state.vars + 1] = {
        name = stmt.name,
        line_start = stmt.line_start,
        line_end = stmt.line_end,
      }
    end
  end

  local function handle_module_call(stmt, state)
    local name = stmt:positional_or_kwarg(1, "name")

    if not name or not name.quoted then
      return
    end

    state.module_decl = {
      name = name.text,
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
  end

  local function handle_bazel_dep(stmt, state)
    local name = stmt:positional_or_kwarg(1, "name")
    local version = stmt:positional_or_kwarg(2, "version")
    local repo_name = stmt:kwarg("repo_name")

    if not name or not name.quoted then
      return
    end

    local apparent_repo_name = name.text

    if repo_name and repo_name.kind == "string" and repo_name.text ~= "" then
      apparent_repo_name = repo_name.text
    elseif repo_name and not repo_name.quoted and repo_name.text == "None" then
      apparent_repo_name = nil
    end

    state.deps[#state.deps + 1] = {
      module_name = name.text,
      apparent_repo_name = apparent_repo_name,
      version = version and util.render_value(version) or '""',
      dev_dependency = dev_dependency(stmt),
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
  end

  local function handle_use_repo(stmt, state)
    local first = stmt.args[1]
    if not first or first.kind ~= "positional" then
      return
    end

    local proxy = first.value
    if proxy.kind ~= "identifier" or not is_alias_of(state, proxy.text, "extension") then
      return
    end

    local names = {}

    for i = 2, #stmt.args do
      local entry = stmt.args[i]

      if entry.kind == "dictionary_splat" then
        for _, key in ipairs(entry.keys) do
          names[#names + 1] = util.quote("@" .. key)
        end
      else
        names[#names + 1] = util.arg_repo_label(entry)
      end
    end

    if #names == 0 then
      return
    end

    push_repo(state, proxy.text, names, stmt.line_start, stmt.line_end)
  end

  -- track_dev=false skips reading dev_dependency: include() doesn't accept it
  -- and we don't want a stale field on its records.
  local function push_targets(items, stmt, track_dev)
    local is_dev = track_dev and dev_dependency(stmt) or false

    for _, entry in ipairs(stmt.args) do
      if entry.kind == "positional" then
        items[#items + 1] = {
          target = util.render_value(entry.value),
          dev_dependency = is_dev,
          line_start = stmt.line_start,
          line_end = stmt.line_end,
        }
      end
    end
  end

  local function handle_extension_tag(stmt, state)
    local alias, tag = stmt.target:match("^(.+)%.(.+)$")

    if not is_alias_of(state, alias, "extension") then
      return false
    end

    state.tags[#state.tags + 1] = {
      alias = alias,
      tag = tag,
      name = stmt:kwarg("name"),
      line_start = stmt.line_start,
      line_end = stmt.line_end,
    }
    return true
  end

  local function handle_repo_rule_alias(stmt, state)
    if not is_alias_of(state, stmt.target, "repo_rule") then
      return
    end

    local name = stmt:kwarg("name")

    if not name then
      return
    end

    local repo_name = name.quoted and util.quote("@" .. name.text) or name.text

    push_repo(state, stmt.target, { repo_name }, stmt.line_start, stmt.line_end, dev_dependency(stmt))
  end

  -- Default for calls whose target isn't a known builtin. Tag form
  -- `<alias>.<tag>` resolves via state.aliases as "extension"; a bare
  -- `<alias>` resolves as "repo_rule". Calls that match neither are
  -- silently dropped.
  local function handle_alias(stmt, state)
    if handle_extension_tag(stmt, state) then
      return
    end

    handle_repo_rule_alias(stmt, state)
  end

  local call_handlers = {
    module = handle_module_call,
    bazel_dep = handle_bazel_dep,
    use_repo = handle_use_repo,
    register_toolchains = function(stmt, state)
      push_targets(state.toolchains, stmt, true)
    end,
    register_execution_platforms = function(stmt, state)
      push_targets(state.exec_platforms, stmt, true)
    end,
    include = function(stmt, state)
      push_targets(state.includes, stmt, false)
    end,
  }

  -- Bzlmod calls we skip from the index. Overrides, injected repos, and flag
  -- aliases are valid but add noise rather than navigable structure.
  local IGNORED_CALLS = {
    single_version_override = true,
    multiple_version_override = true,
    archive_override = true,
    git_override = true,
    local_path_override = true,
    override_repo = true,
    flag_alias = true,
    inject_repo = true,
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

    handle_alias(stmt, state)
  end

  local function render_module(state)
    if not state.module_decl then
      return nil
    end

    -- Alternative without R.render_items (single-item section):
    --   local decl = state.module_decl
    --   return "module:\n" .. R.item_line(util.quote("@" .. decl.name), R.item_range(decl)) .. "\n"
    return R.render_items("module", { state.module_decl }, function(decl)
      return R.item_line(util.quote("@" .. decl.name), R.item_range(decl))
    end)
  end

  local function render_deps(state)
    return R.render_items("bazel_deps", state.deps, function(dep)
      local label = dep.apparent_repo_name and util.quote("@" .. dep.apparent_repo_name) or util.quote(dep.module_name)
      local no_repo_text = dep.apparent_repo_name and "" or ", repo_name=None"
      local value = dep.version .. no_repo_text .. dev_text(dep, ", dev=True")

      return R.label_line(label, value, R.item_range(dep))
    end)
  end

  local function render_extensions(state)
    return R.render_items("module_extensions", state.extensions, function(ext)
      local value = ext.path .. ", " .. ext.name .. dev_text(ext, ", dev=True")

      return R.label_line(ext.alias, value, R.item_range(ext))
    end)
  end

  local function render_tags(state)
    return R.render_items("module_extensions.tags", state.tags, function(tag)
      local prefix = tag.alias .. "." .. tag.tag
      local value = tag.name and util.render_value(tag.name) or nil

      return R.label_line(prefix, value, R.item_range(tag))
    end)
  end

  local function render_repos(state)
    return R.render_items("repos", state.repos, function(repo)
      local value = table.concat(repo.names, ", ") .. dev_text(repo, ", dev=True")

      return R.label_line(repo.rule, value, R.item_range(repo))
    end)
  end

  -- For toolchains and exec_platforms dev_dependency may be true; for includes
  -- it is always false (push_targets is called with track_dev=false), so
  -- dev_text returns "" and the dev marker is omitted.
  local function render_target_section(header, items)
    return R.render_items(header, items, function(item)
      return R.label_line(item.target, dev_text(item, "dev=True"), R.item_range(item))
    end)
  end

  local function render_vars(state)
    return R.render_items("vars", state.vars, function(var)
      return R.label_line(var.name, nil, R.item_range(var))
    end)
  end

  local function new_state()
    return {
      module_decl = nil,
      deps = {},
      extensions = {},
      tags = {},
      repos = {},
      toolchains = {},
      exec_platforms = {},
      vars = {},
      includes = {},
      -- alias name -> "extension" or "repo_rule". Seeded by handle_assignment
      -- when it sees `name = use_extension(...)` / `name = use_repo_rule(...)`,
      -- and consulted later to recognize tag/repo-rule call shapes.
      aliases = {},
    }
  end

  local function render(state, collected)
    local sections = {}

    R.append_section_if_present(sections, R.render_doc("module", collected))
    R.append_section_if_present(sections, render_module(state))
    R.append_section_if_present(sections, render_deps(state))
    R.append_section_if_present(sections, render_extensions(state))
    R.append_section_if_present(sections, render_tags(state))
    R.append_section_if_present(sections, render_repos(state))
    R.append_section_if_present(sections, render_target_section("register_toolchains", state.toolchains))
    R.append_section_if_present(sections, render_target_section("register_execution_platforms", state.exec_platforms))
    R.append_section_if_present(sections, render_vars(state))
    R.append_section_if_present(sections, render_target_section("includes", state.includes))

    return table.concat(sections, "\n")
  end

  return {
    extract = function(source, root)
      local state = new_state()
      local collected = A.collect(root, source)

      for _, stmt in ipairs(collected.statements) do
        if stmt.kind == "assignment" then
          handle_assignment(stmt, state)
        elseif stmt.kind == "call" and stmt.target then
          handle_call(stmt, state)
        end
      end

      return render(state, collected)
    end,
  }
end
