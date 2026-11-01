-- Cursor subscription as a model. Tool calls come back here and maki runs them.
-- Each process starts its own proxy on a free port. The registered URL is only
-- a placeholder until `auth` returns the real one.

local PLACEHOLDER_PORT = 9
local NODE_FLAG = "--experimental-strip-types"
local PROXY_JOB = "cursor-proxy"

local bound_port = nil
local bound_token = nil

local function base_url()
  return "http://127.0.0.1:" .. (bound_port or PLACEHOLDER_PORT) .. "/v1"
end

local function plugin_dir()
  local dir = maki.plugin_dir
  if type(dir) ~= "string" or dir == "" then
    error("cursor provider has no install directory")
  end
  return dir
end

local function proxy_root()
  return plugin_dir() .. "/proxy"
end

local function conversations_dir()
  return plugin_dir() .. "/conversations"
end

local function proxy_env(extra)
  local env = {
    MAKI_CURSOR_CONVERSATIONS = conversations_dir(),
  }
  if bound_port then
    env.MAKI_CURSOR_PORT = tostring(bound_port)
  end
  if bound_token then
    env.MAKI_CURSOR_TOKEN = bound_token
  end
  if extra then
    for key, value in pairs(extra) do
      env[key] = value
    end
  end
  return env
end

local function node_bin()
  return maki.uv.os_getenv("MAKI_CURSOR_NODE") or "node"
end

local function argv(command, extra)
  local cmd = {
    node_bin(),
    "--disable-warning=ExperimentalWarning",
    NODE_FLAG,
    proxy_root() .. "/src/cli.ts",
    command,
  }
  if extra then
    for _, item in ipairs(extra) do
      cmd[#cmd + 1] = item
    end
  end
  return cmd
end

local function run(command, extra, timeout_ms)
  local id, err = maki.fn.jobstart(argv(command, extra), {
    cwd = proxy_root(),
    env = proxy_env(),
  })
  if not id then
    return nil, err
  end
  return maki.fn.jobwait(id, timeout_ms or 30000)
end

local function trimmed(text)
  return (text or ""):gsub("%s+$", "")
end

local function failed(result, fallback)
  if not result then
    return fallback
  end
  if result.stderr and result.stderr ~= "" then
    return result.stderr
  end
  return fallback
end

local function ensure_deps()
  local result, err = run("ensure", nil, 180000)
  if not result then
    return nil, err
  end
  if result.exit_code ~= 0 then
    return nil, failed(result, "npm ci failed")
  end
  return true
end

local function server_up()
  if not bound_port or not bound_token then
    return false
  end
  local result = run("health", nil, 2000)
  return result and result.exit_code == 0
end

local function stop_proxy()
  local id = maki.fn.jobfind(PROXY_JOB)
  if id then
    maki.fn.jobstop(id)
  end
  bound_port = nil
  bound_token = nil
end

local function ensure_running()
  if server_up() then
    return true
  end
  stop_proxy()
  local nonce, nonce_err = run("nonce", nil, 5000)
  if not nonce or nonce.exit_code ~= 0 then
    return nil, nonce_err or failed(nonce, "could not start the cursor proxy")
  end
  local ready = proxy_root() .. "/ready-" .. trimmed(nonce.stdout) .. ".json"
  local _, start_err = maki.fn.jobstart(argv("serve"), {
    cwd = proxy_root(),
    scope = "plugin",
    name = PROXY_JOB,
    env = proxy_env({
      MAKI_CURSOR_PORT = "0",
      MAKI_CURSOR_READY = ready,
    }),
  })
  if start_err then
    return nil, start_err
  end
  local waited, wait_err = run("wait-ready", { ready }, 12000)
  if not waited or waited.exit_code ~= 0 then
    stop_proxy()
    return nil, wait_err or failed(waited, "cursor proxy did not become ready")
  end
  local port_text, token = trimmed(waited.stdout):match("^(%d+)%s+(%S+)$")
  if not port_text or not token then
    stop_proxy()
    return nil, "cursor proxy did not report a port"
  end
  bound_port = tonumber(port_text)
  bound_token = token
  return true
end

local function store_login(text)
  local creds, err = maki.json.decode(text)
  if not creds then
    return nil, err or "cursor login returned nothing"
  end
  local ok, set_err = maki.provider.auth.set("cursor", {
    access = creds.access,
    refresh = creds.refresh,
    expires = creds.expires,
  })
  if not ok then
    return nil, set_err or "could not store the cursor login"
  end
  return true
end

maki.provider.register({
  slug = "cursor",
  display_name = "Cursor",
  codec = "openai",
  base_url = base_url(),
  default_model = "composer-2.5",
  family = "generic",
  accepts_arbitrary_models = true,
  max_output_tokens = 64000,
  context_window = 200000,
  docs = {
    features = "Cursor subscription models. Maki runs the tools. The address above is a placeholder; each process listens on its own port.",
    discovery_note = "Models come from the signed-in Cursor account. The list below is the offline fallback.",
  },
  openai = { session_id = { header = "x-maki-session" } },
  models = {
    {
      prefixes = { "composer-2.5" },
      tier = "strong",
      default = true,
      supports_thinking = true,
    },
    {
      prefixes = { "auto" },
      tier = "medium",
    },
  },

  build_body = function(_, body)
    body.thinking = nil
    body.reasoning_effort = nil
    return body
  end,

  auth = function(_, purpose)
    local logged = run("status", nil, 5000)
    if not logged or logged.exit_code ~= 0 then
      return nil, "run `maki auth login cursor`"
    end
    local deps = run("deps", nil, 5000)
    if not deps or deps.exit_code ~= 0 then
      return nil, "run `maki auth login cursor`"
    end
    if purpose == "refresh" then
      local refreshed, refresh_err = run("refresh", nil, 30000)
      if not refreshed or refreshed.exit_code ~= 0 then
        return nil, refresh_err or failed(refreshed, "cursor token refresh failed")
      end
      local stored, store_err = store_login(trimmed(refreshed.stdout))
      if not stored then
        return nil, store_err
      end
    end
    local running, run_err = ensure_running()
    if not running then
      return nil, run_err
    end
    return {
      base_url = base_url(),
      headers = { Authorization = "Bearer " .. bound_token },
    }
  end,

  list_models = function(ctx)
    local parsed, err = ctx.get_json("/models")
    if err then
      return nil, err
    end
    local rows = {}
    for _, model in ipairs(parsed.data or {}) do
      if type(model.id) == "string" and model.id ~= "" then
        rows[#rows + 1] = {
          id = model.id,
          context_window = 200000,
          max_output_tokens = 64000,
        }
      end
    end
    return rows
  end,

  login = function(ctx)
    local ready, err = ensure_deps()
    if not ready then
      error(err)
    end
    local state = proxy_root() .. "/login-state.json"
    local url_result, url_err = run("login-url", { state }, 15000)
    if not url_result or url_result.exit_code ~= 0 then
      error(url_err or failed(url_result, "could not start cursor login"))
    end
    ctx.print(trimmed(url_result.stdout))
    ctx.open_url(trimmed(url_result.stdout))
    ctx.print("Finish signing in in the browser. This waits until Cursor returns a token.")
    local poll, poll_err = run("login-poll", { state }, 300000)
    if not poll or poll.exit_code ~= 0 then
      error(poll_err or failed(poll, "cursor login failed"))
    end
    local stored, store_err = store_login(trimmed(poll.stdout))
    if not stored then
      error(store_err)
    end
  end,

  logout = function()
    stop_proxy()
    run("logout", nil, 5000)
  end,
})
