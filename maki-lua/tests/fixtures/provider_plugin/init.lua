-- An OpenAI-compatible provider that uses every `maki.provider.register` hook
-- once. The provider docs quote this file as their worked example.

local ANONYMOUS = "anonymous"
local REFRESH = "refresh"
local MODELS_PATH = "/models"

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
  -- Prepended to maki's system prompt.
  system_prefix = "Acme house rules: answer in full sentences.",
  models = {
    {
      prefixes = { "acme-1", "acme" },
      tier = "strong",
      context_window = 200000,
      max_output_tokens = 8192,
      supports_thinking = true,
      -- The only levels Acme accepts. Maki snaps any other level onto them.
      thinking_fields = {
        low = { reasoning_effort = "low" },
        high = { reasoning_effort = "high" },
      },
    },
  },

  -- `purpose` is "resolve" before the first request, "reload" after a login
  -- in another process changed the store, and "refresh" after a 401. An Acme
  -- token is single use, so a refresh mints and stores the next one here.
  auth = function(ctx, purpose)
    if purpose ~= REFRESH then
      return bearer(stored_token(ctx.slug))
    end
    local renewed = stored_token(ctx.slug) .. "-renewed"
    maki.provider.auth.set(ctx.slug, { token = renewed })
    return bearer(renewed)
  end,

  -- The catalogue changes faster than this file, so the picker asks the API.
  -- `ctx.get_json` uses the chat requests' origin and headers.
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

  -- Gets the final body, thinking level included. Acme wants the effort under
  -- its own key. `opts.thinking` is nil when thinking is off.
  build_body = function(_, body, model, opts)
    body.acme_reasoning = { model = model, effort = body.reasoning_effort, asked_for = opts.thinking }
    body.reasoning_effort = nil
    return body
  end,

  -- Acme answers 429 for a spent monthly allowance, which no retry can fix,
  -- so a 400 stops the retries.
  map_error = function(_, status, message)
    if status == 429 and message:find("allowance") then
      return { status = 400, message = "Acme allowance is spent until the next cycle" }
    end
  end,

  fetch_usage = function()
    return { plan = "team", limits = { { label = "Monthly allowance", percentage = 42 } } }
  end,

  -- Defining `login` lists the provider in `maki auth login`.
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
