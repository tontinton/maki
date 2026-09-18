-- An OpenAI-compatible provider written entirely in Lua.
--
-- Every hook `maki.provider.register` accepts appears once, with the reason it
-- exists, so this file doubles as the worked example in the plugin docs.

local SLUG = "acmelua"
local ANONYMOUS = "anonymous"
local BASE_URL = maki.uv.os_getenv("ACME_BASE_URL") or "https://api.acme.example/v1"

-- The token `login` stored, or none at all.
local function stored_token()
  local stored = maki.provider.auth.get(SLUG)
  return (stored and stored.token) or ANONYMOUS
end

local function lease(token)
  return { base_url = BASE_URL, headers = { authorization = "Bearer " .. token } }
end

maki.provider.register({
  slug = SLUG,
  display_name = "Acme (Lua)",
  codec = "openai",
  base_url = BASE_URL,
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

  -- Called once, lazily, before the first request of the session.
  resolve_auth = function()
    return lease(stored_token())
  end,

  -- Called after a 401 that arrived before any output. An Acme lease is single
  -- use, so the stored credential buys the next one instead of being resent,
  -- and the new one is stored right here: maki holds this provider's credential
  -- lock while the hook runs and lets the hook itself back in through it.
  refresh_auth = function()
    local renewed = stored_token() .. "-renewed"
    maki.provider.auth.set(SLUG, { token = renewed })
    return lease(renewed)
  end,

  -- Called when the store changed underneath us, e.g. after `maki auth login`
  -- ran in another process.
  reload_auth = function()
    return lease(stored_token())
  end,

  -- Acme's catalogue moves faster than this file, so the picker asks the API.
  list_models = function()
    return { { id = "acme-1", context_window = 200000, tier = "strong" } }
  end,

  -- Runs on the final body, after maki rendered the thinking level into it, so
  -- what arrives here is exactly what goes on the wire. Acme wants the effort
  -- under its own key and rejects OpenAI's.
  build_body = function(body, model, opts)
    body.acme_reasoning = { model = model, effort = body.reasoning_effort, asked_for = opts.thinking }
    body.reasoning_effort = nil
    return body
  end,

  -- Acme answers 429 for a spent monthly allowance, which no retry can fix.
  map_error = function(status, message)
    if status == 429 and message:find("allowance") then
      return { status = 400, message = "Acme allowance is spent until the next cycle" }
    end
  end,

  fetch_usage = function()
    return { plan = "team", limits = { { label = "Monthly allowance", percentage = 42 } } }
  end,

  -- Having a `login` is what makes this provider an auth target: it shows up in
  -- `maki auth login` because this function exists.
  login = function(ctx)
    local key = ctx.prompt({ label = "Acme API key: ", secret = true })
    if not key or key == "" then
      ctx.print("No key entered, nothing was stored.")
      return
    end
    maki.provider.auth.set(SLUG, { token = key })
    ctx.print("Stored your Acme key.")
  end,

  logout = function(ctx)
    maki.provider.auth.clear(SLUG)
    ctx.print("Forgot your Acme key.")
  end,
})
