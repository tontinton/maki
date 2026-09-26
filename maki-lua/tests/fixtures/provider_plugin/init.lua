-- An OpenAI-compatible provider written entirely in Lua.
--
-- Every hook `maki.provider.register` accepts appears once, with the reason it
-- exists, so this file doubles as the worked example in the plugin docs. Each
-- one is handed a `ctx` first, which knows the slug, the origin in force and
-- the headers every request carries.

local ANONYMOUS = "anonymous"
local REFRESH = "refresh"
local MODELS_PATH = "/models"

-- The token `login` stored, or none at all.
local function stored_token(slug)
  local stored = maki.provider.auth.get(slug)
  return (stored and stored.token) or ANONYMOUS
end

local function bearer(token)
  return { headers = { authorization = "Bearer " .. token } }
end

maki.provider.register({
  slug = "acmelua",
  display_name = "Acme (Lua)",
  codec = "openai",
  base_url = "https://api.acme.example/v1",
  -- Prepended to whatever system prompt maki assembled, so house rules the
  -- provider needs ride along without the agent having to know about them.
  system_prefix = "Acme house rules: answer in full sentences.",
  models = {
    {
      prefixes = { "acme-1", "acme" },
      tier = "strong",
      context_window = 200000,
      max_output_tokens = 8192,
      supports_thinking = true,
      -- The only two levels Acme accepts; maki snaps anything else onto them.
      thinking_fields = {
        low = { reasoning_effort = "low" },
        high = { reasoning_effort = "high" },
      },
    },
  },

  -- `purpose` says why maki asks: "resolve" once, lazily, before the first
  -- request of the session, "reload" when the store changed underneath us
  -- (after `maki auth login` ran in another process), and "refresh" after a
  -- 401 that arrived before any output. An Acme lease is single use, so a
  -- refresh buys the next one with the stored credential instead of resending
  -- it, and stores it right here: maki holds this provider's credential lock
  -- while the hook runs and lets the hook itself back in through it.
  auth = function(ctx, purpose)
    if purpose ~= REFRESH then
      return bearer(stored_token(ctx.slug))
    end
    local renewed = stored_token(ctx.slug) .. "-renewed"
    maki.provider.auth.set(ctx.slug, { token = renewed })
    return bearer(renewed)
  end,

  -- Acme's catalogue moves faster than this file, so the picker asks the API.
  -- `ctx.get_json` goes where the chat requests go, with their headers, and a
  -- failure it hands back is returned as the hook's own.
  list_models = function(ctx)
    local body, err = ctx.get_json(MODELS_PATH)
    if err then
      return nil, err
    end
    local models = {}
    for _, m in ipairs(body.data or {}) do
      table.insert(models, { id = m.id, context_window = m.context_length, tier = "strong" })
    end
    return models
  end,

  -- Runs on the final body, after maki rendered the thinking level into it, so
  -- what arrives here is exactly what goes on the wire. Acme wants the effort
  -- under its own key and rejects OpenAI's. `opts.thinking` is nil when
  -- thinking is off.
  build_body = function(_, body, model, opts)
    body.acme_reasoning = { model = model, effort = body.reasoning_effort, asked_for = opts.thinking }
    body.reasoning_effort = nil
    return body
  end,

  -- Acme answers 429 for a spent monthly allowance, which no retry can fix.
  map_error = function(_, status, message)
    if status == 429 and message:find("allowance") then
      return { status = 400, message = "Acme allowance is spent until the next cycle" }
    end
  end,

  fetch_usage = function()
    return { plan = "team", limits = { { label = "Monthly allowance", percentage = 42 } } }
  end,

  -- Having a `login` is what makes this provider an auth target: it shows up in
  -- `maki auth login` because this function exists. Its `ctx` can also talk to
  -- the terminal.
  login = function(ctx)
    local key = ctx.prompt({ label = "Acme API key: ", secret = true })
    if not key or key == "" then
      ctx.print("No key entered, nothing was stored.")
      return
    end
    maki.provider.auth.set(ctx.slug, { token = key })
    ctx.print("Stored your Acme key.")
  end,

  logout = function(ctx)
    maki.provider.auth.clear(ctx.slug)
    ctx.print("Forgot your Acme key.")
  end,
})
