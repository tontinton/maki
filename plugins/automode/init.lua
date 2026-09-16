-- Automode: a chain of reviewer models decides the tool calls that would
-- otherwise stop and ask you. Cheapest link first; anything it is unsure
-- about escalates to the next one, and an exhausted chain falls back to the
-- normal prompt, so being unsure is never the same as being approved.
--
-- Every review is a model call you pay for, which is why this plugin is
-- bundled but off: it needs `plugins.automode = { enabled = true }` and a
-- chain before it does anything at all.
--
-- /automode toggles it, shows counters, edits the chain, or opens the
-- verdict log with the exact request each reviewer was given.

local ListPicker = require("maki.list_picker")
local Toast = require("maki.toast")
local Helpers = require("automode_helpers")
local DEFAULT_POLICY = require("policy")

local opts = maki.api.register_options({
  chain = {
    default = "",
    type = "string",
    desc = 'Reviewer models, comma-separated and cheapest first (e.g. "anthropic/claude-haiku-4-5-20251001"). Empty leaves automode inert.',
  },
  timeout_ms = {
    default = 15000,
    min = 1,
    desc = "How long a reviewer link has to answer before the call escalates to the next one.",
  },
})

local PROJECT_POLICY_HEADER = "\n\n## Project rules\n\n"
local NO_CHAIN_HINT = "no chain — /automode model, or set plugins.automode.chain"
local OVERRIDE_TIMEOUT_MS = 1000

local statePath = maki.fs.joinpath(maki.env.state_dir(), "automode.json")

local counts = { allowed = 0, denied = 0, escalated = 0, prompted = 0, redirected = 0 }
local spent = 0
-- Newest last; one entry per link verdict, so a two-link chain that escalates
-- leaves two. Bounded so a long session cannot grow it without limit.
local HISTORY_MAX = 40
local history = {}

local state = {}
local stateLoaded = false
local policyCache = { key = nil, text = nil }

local function trim(s)
  return (s or ""):match("^%s*(.-)%s*$")
end

-- The denial this session for the same executable that the user has not yet
-- answered with a go-ahead.
local function last_denial_for(exe)
  for i = #history, 1, -1 do
    local h = history[i]
    if h.resolution == "denied" and h.executable == exe then
      return h
    end
  end
  return nil
end

-- A model link sees only the latest user message, so "try again" after a
-- denial reads as an unprompted call and is denied again. This handler runs
-- ahead of the models and turns that exchange into the override the policy
-- already promises: the user answered a denial with a go-ahead, so the
-- retried command is theirs to authorise. Scoped to the executable the denial
-- was about, so the override cannot leak onto unrelated calls in the turn.
local function retry_override(call)
  -- MCP calls carry no scopes; their tool key is the equivalent unit.
  local exe = Helpers.executable_of(call.scopes) or call.tool
  if not exe then
    return "ASK"
  end
  local denial = last_denial_for(exe)
  if not denial or denial.overridden then
    return "ASK"
  end
  if not Helpers.is_go_ahead(call.last_user_message) then
    return "ASK"
  end
  denial.overridden = true
  return "ALLOW", "user answered the earlier denial of `" .. exe .. "` with a go-ahead"
end

-- Lazy: async fs at plugin top level aborts the whole load.
local function ensure_state()
  if stateLoaded then
    return
  end
  stateLoaded = true
  local raw = maki.fs.read(statePath)
  if not raw then
    return
  end
  local ok, decoded = pcall(maki.json.decode, raw)
  if ok and type(decoded) == "table" then
    state = { enabled = decoded.enabled, chain = decoded.chain }
  end
end

local function save_state()
  maki.fs.write(statePath, maki.json.encode(state))
end

local function is_enabled()
  ensure_state()
  if state.enabled ~= nil then
    return state.enabled
  end
  return true
end

-- The config seeds the chain; a pick in /automode model replaces it and
-- outlives the config value, because the picker is the more specific answer.
local function chain_specs()
  ensure_state()
  if type(state.chain) == "table" then
    return state.chain
  end
  return Helpers.parse_chain(opts.chain)
end

-- Returns (text, changed): mtime-keyed so TurnStart re-registers only when a
-- policy file actually changed.
local function policy_text()
  local global = maki.fs.joinpath(maki.env.config_dir(), "automode-policy.md")
  local cwd = maki.uv.cwd()
  local project = cwd and maki.fs.joinpath(cwd, ".maki", "automode.md") or nil
  local key = ""
  for _, path in ipairs({ global, project }) do
    local meta = maki.fs.metadata(path)
    key = key .. path .. ":" .. tostring(meta and meta.mtime or "-") .. ";"
  end
  if policyCache.key == key then
    return policyCache.text, false
  end
  local text = maki.fs.read(global) or DEFAULT_POLICY
  if project then
    local rules = maki.fs.read(project)
    if rules then
      text = text .. PROJECT_POLICY_HEADER .. rules
    end
  end
  policyCache.key, policyCache.text = key, text
  return text, true
end

-- Same-name registration replaces, so only links beyond the new chain length
-- need explicit removal. Never clear_reviewers: another plugin may own links
-- of its own and they are not ours to drop.
local registered = 0

local function register_chain()
  local specs = chain_specs()
  local text = policy_text()
  maki.api.register_reviewer({
    name = "automode-override",
    order = -1,
    handler = retry_override,
    timeout_ms = OVERRIDE_TIMEOUT_MS,
  })
  for i, spec in ipairs(specs) do
    maki.api.register_reviewer({
      name = "automode-" .. i,
      model = spec,
      policy = text,
      timeout_ms = opts.timeout_ms,
      order = i,
    })
  end
  for i = #specs + 1, registered do
    maki.api.unregister_reviewer("automode-" .. i)
  end
  registered = #specs
  return #specs
end

local function teardown()
  maki.api.unregister_reviewer("automode-override")
  for i = 1, registered do
    maki.api.unregister_reviewer("automode-" .. i)
  end
  registered = 0
end

local function sync()
  if is_enabled() and register_chain() > 0 then
    maki.ui.set_status_hint({ { "⚡", "automode" } })
  else
    teardown()
    maki.ui.set_status_hint(nil)
  end
end

local function chain_lines(specs)
  if #specs == 0 then
    return NO_CHAIN_HINT
  end
  local lines = {}
  for i, spec in ipairs(specs) do
    lines[#lines + 1] = i .. ". " .. spec
  end
  return table.concat(lines, "\n")
end

local function toggle()
  state.enabled = not is_enabled()
  save_state()
  sync()
  if not state.enabled then
    Toast.show("off — normal prompting", { title = "automode" })
  else
    Toast.show("on\n" .. chain_lines(chain_specs()), { title = "automode" })
  end
end

local function status()
  Toast.show(
    (is_enabled() and "on" or "off")
      .. string.format(
        " · allow %d deny %d prompt %d redirect %d · $%.4f",
        counts.allowed,
        counts.denied,
        counts.prompted,
        counts.redirected,
        spent
      )
      .. "\n"
      .. chain_lines(chain_specs()),
    { title = "automode", timeout_secs = 6 }
  )
end

-- Native picker; enter toggles chain membership and reopens so the markers
-- reflect the change, esc closes.
local function edit_chain()
  local models, err = maki.model.available()
  if not models then
    maki.ui.flash("automode: " .. tostring(err))
    return
  end
  local specs = chain_specs()
  while true do
    local in_chain = {}
    for i, spec in ipairs(specs) do
      in_chain[spec] = i
    end
    local items = {}
    for _, spec in ipairs(models) do
      local pos = in_chain[spec]
      items[#items + 1] = {
        label = spec,
        detail = pos and ("#" .. pos .. " in chain") or nil,
      }
    end
    local res = ListPicker.open(items, { title = " Automode Chain " })
    if res.type ~= "choice" then
      break
    end
    local spec = items[res.index].label
    if in_chain[spec] then
      table.remove(specs, in_chain[spec])
    else
      specs[#specs + 1] = spec
    end
    state.chain = specs
    save_state()
    sync()
  end
  Toast.show(chain_lines(specs), { title = "automode chain" })
end

-- The UI only fires this event; presentation is ours.
maki.api.create_autocmd("ToolReviewed", {
  callback = function(ev)
    local data = ev.data or {}
    if data.resolution and counts[data.resolution] then
      counts[data.resolution] = counts[data.resolution] + 1
    end
    spent = spent + (tonumber(data.cost) or 0)
    history[#history + 1] = {
      at = os.date("%H:%M:%S"),
      tool = data.tool,
      reviewer = data.reviewer,
      model = data.model,
      verdict = data.verdict,
      reason = data.reason,
      resolution = data.resolution,
      cost = tonumber(data.cost) or 0,
      scopes = data.scopes,
      executable = Helpers.executable_of(data.scopes) or data.tool,
      request = data.request,
    }
    if #history > HISTORY_MAX then
      table.remove(history, 1)
    end
    if data.resolution == "denied" then
      Toast.show((data.tool or "?") .. " " .. (data.reason or "denied"), { title = "automode" })
    elseif data.resolution == "redirected" then
      Toast.show((data.tool or "?") .. " redirected: told to try another approach", { title = "automode" })
    end
  end,
})

-- Policy edits land without a /reload.
maki.api.create_autocmd("TurnStart", {
  callback = function()
    if not is_enabled() then
      return
    end
    local _, changed = policy_text()
    if changed then
      register_chain()
    end
  end,
})

maki.api.create_autocmd({ "SessionFocusChanged", "TurnStart" }, {
  once = true,
  callback = function()
    maki.async.run(function()
      sync()
      if not is_enabled() then
        maki.ui.flash("automode is off — /automode to enable")
      elseif #chain_specs() == 0 then
        maki.ui.flash("automode has " .. NO_CHAIN_HINT)
      end
    end)
  end,
})

-- Inspector: a scrollable window over the verdict history. The list shows one
-- line per link verdict; the detail pane below shows the selected entry's
-- reason, scopes, and the verbatim request the reviewer was given, so a
-- surprising ASK can be traced to what it did (or didn't) know.
local function inspect_lines(sel, width)
  local lines = {}
  local function add(s)
    lines[#lines + 1] = s
  end
  local specs = chain_specs()
  add(
    string.format(
      "%s · allow %d deny %d escalate %d prompt %d redirect %d · $%.4f · chain: %s",
      is_enabled() and "on" or "off",
      counts.allowed,
      counts.denied,
      counts.escalated,
      counts.prompted,
      counts.redirected,
      spent,
      #specs > 0 and table.concat(specs, " → ") or "none"
    )
  )
  add("")
  if #history == 0 then
    add("no verdicts yet this session")
    return lines, 1
  end
  local list_start = #lines + 1
  for i = #history, 1, -1 do
    local h = history[i]
    local mark = (i == sel) and "▶" or " "
    add(
      string.format(
        "%s %s %-5s %-10s %-9s %-12s %s",
        mark,
        h.at,
        h.verdict or "?",
        h.resolution or "?",
        h.tool or "?",
        (h.reviewer ~= "" and h.reviewer) or "(maki)",
        h.reason and ("— " .. h.reason) or ""
      )
    )
  end
  local h = history[sel]
  add(string.rep("─", width))
  add(
    string.format(
      "%s · %s · $%.4f",
      h.reviewer ~= "" and h.reviewer or "(maki)",
      h.model ~= "" and h.model or "no link",
      h.cost
    )
  )
  if h.scopes and #h.scopes > 0 then
    add("scopes: " .. table.concat(h.scopes, " ; "))
  end
  add("")
  if h.request and h.request ~= "" then
    for line in (h.request .. "\n"):gmatch("(.-)\n") do
      add(line)
    end
  else
    add("(no request: maki synthesised this verdict after the chain was exhausted)")
  end
  -- Cursor row of the selected list line, for set_cursor.
  return lines, list_start + (#history - sel)
end

local function inspect()
  local buf = maki.ui.buf()
  local width, height = 110, 40
  local sel = #history
  local lines, row = inspect_lines(sel, width)
  buf:set_lines(lines)
  local win = maki.ui.open_win(buf, {
    title = "Automode",
    border = "rounded",
    width = width,
    height = height,
    footer = { { "j/k", "select" }, { "q", "close" } },
  })
  win:set_cursor(row)
  while true do
    local ev = win:recv(1000)
    if not ev or ev.type == "close" then
      break
    end
    if ev.type == "key" then
      if ev.key == "q" or ev.key == "esc" then
        break
      elseif (ev.key == "j" or ev.key == "down") and sel > 1 then
        sel = sel - 1
      elseif (ev.key == "k" or ev.key == "up") and sel < #history then
        sel = sel + 1
      end
    end
    if #history > 0 then
      sel = math.min(math.max(sel, 1), #history)
    end
    lines, row = inspect_lines(sel, width)
    buf:set_lines(lines)
    win:set_cursor(row)
  end
  win:close()
end

maki.api.register_command({
  name = "/automode",
  description = "Toggle automode; 'status' counters, 'model' edits the chain, 'inspect' shows verdicts with what each reviewer saw",
  nargs = "?",
  handler = function(cmd)
    ensure_state()
    local arg = trim(cmd and cmd.args or "")
    if arg == "" or arg == "toggle" then
      toggle()
    elseif arg == "model" then
      edit_chain()
    elseif arg == "inspect" then
      maki.async.run(inspect)
    else
      status()
    end
  end,
})
